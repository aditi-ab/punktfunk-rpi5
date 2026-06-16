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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
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
    QueryDisplayConfig, SetDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS, SDC_ALLOW_CHANGES, SDC_APPLY,
    SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_GLOBAL, CDS_NORESET,
    CDS_TEST, CDS_TYPE, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW,
    DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
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
const IOCTL_DRIVER_PING: u32 = ctl(0x888);
const IOCTL_GET_VERSION: u32 = ctl(0x8FF);

// A fixed monitor identity. One session at a time today; Windows persists this monitor's layout
// across sessions by GUID, and REMOVE keys off it. (TODO: derive per-client when concurrent
// sessions land.)
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
unsafe fn resolve_render_adapter_luid() -> Option<LUID> {
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
        if best.as_ref().map_or(true, |(_, v, _)| vram > *v) {
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
            let rc = DisplayConfigSetDeviceInfo(&mut s.header);
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
        ChangeDisplaySettingsExW(PCWSTR(wname.as_ptr()), Some(&dm), None, CDS_UPDATEREGISTRY, None)
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

/// Detach every display except `keep_gdi_name`, leaving the SudoVDA virtual output as the ONLY
/// display. This is the SudoVDA/Apollo "isolate the virtual display" move and the key to capturing
/// the secure desktop: Windows renders the login / UAC (Winlogon) desktop on the physical/primary
/// display and resets the topology when it switches there — with a physical monitor still attached
/// (e.g. an LG TV), the login lands on it and our virtual output goes perpetually ACCESS_LOST. With
/// the physical detached and the change PERSISTED to the registry, Winlogon reads "only the virtual
/// is attached" and the secure desktop has nowhere to render but the output we capture.
///
/// Returns the displays we detached plus their saved modes so teardown can restore them.
///
/// Superseded by the atomic CCD [`isolate_displays_ccd`] (the legacy per-device GDI detach misses
/// iGPU-attached monitors on a hybrid box and churns the topology). Retained for reference / a
/// possible fallback.
#[allow(dead_code)]
unsafe fn isolate_displays(keep_gdi_name: &str) -> Vec<(String, DEVMODEW)> {
    let mut saved = Vec::new();
    let mut idx = 0u32;
    loop {
        let mut dd = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !EnumDisplayDevicesW(PCWSTR::null(), idx, &mut dd, 0).as_bool() {
            break;
        }
        idx += 1;
        if (dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP).0 == 0 {
            continue; // not part of the desktop — nothing to detach
        }
        let name = String::from_utf16_lossy(&dd.DeviceName);
        let name = name.trim_end_matches('\u{0}').to_string();
        if name == keep_gdi_name {
            continue; // the virtual output we want to keep
        }
        // Save the current mode so the teardown can re-attach this display where it was.
        let mut cur = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let wname: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        if EnumDisplaySettingsW(PCWSTR(wname.as_ptr()), ENUM_CURRENT_SETTINGS, &mut cur).as_bool() {
            saved.push((name.clone(), cur));
        }
        // A 0x0 mode removes the display from the desktop. NORESET batches; we commit once below.
        let off = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            dmFields: DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT,
            ..Default::default()
        };
        let r = ChangeDisplaySettingsExW(
            PCWSTR(wname.as_ptr()),
            Some(&off),
            None,
            CDS_UPDATEREGISTRY | CDS_NORESET | CDS_GLOBAL,
            None,
        );
        tracing::info!("display isolate: detaching {name} (result={})", r.0);
    }
    if !saved.is_empty() {
        // Commit the batched detaches (NULL device + 0 flags applies the pending registry changes).
        let _ = ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None);
        tracing::info!(
            "display isolate: {} display(s) detached — only {keep_gdi_name} remains",
            saved.len()
        );
    }
    saved
}

/// Re-attach the displays [`isolate_displays`] detached, restoring each to its saved mode. Called on
/// teardown BEFORE the virtual output is removed, so there is always at least one display.
unsafe fn restore_displays(saved: &[(String, DEVMODEW)]) {
    for (name, dm) in saved {
        let wname: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = ChangeDisplaySettingsExW(
            PCWSTR(wname.as_ptr()),
            Some(dm),
            None,
            CDS_UPDATEREGISTRY | CDS_NORESET | CDS_GLOBAL,
            None,
        );
    }
    if !saved.is_empty() {
        let _ = ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None);
        tracing::info!("display isolate: restored {} display(s)", saved.len());
    }
}

/// Saved active display topology, for restoring on teardown.
type SavedConfig = (Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>);

/// `DISPLAYCONFIG_PATH_ACTIVE` (wingdi.h) — the `flags` bit marking a path active. The `windows` crate
/// doesn't export it, so define it here.
const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;

/// Robust display isolation via the CCD API. The legacy [`isolate_displays`] (EnumDisplayDevices +
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
        tracing::info!("display isolate (CCD): SudoVDA target {keep_target_id} already the only active display");
        return Some(saved);
    }
    let rc = SetDisplayConfig(
        Some(paths.as_slice()),
        Some(modes.as_slice()),
        SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
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

/// Re-detach physical displays so the secure (Winlogon) desktop keeps rendering to the virtual
/// output — for the in-session DXGI capture recovery (dxgi.rs `recreate_dupl`). The lock/UAC/login
/// switch can re-attach a physical monitor (the secure desktop then lands on IT and our virtual
/// output goes perpetually ACCESS_LOST — the "born-lost" storm); re-running the isolate routes the
/// secure desktop back to the virtual output, mirroring what a fresh session's `create` does (the
/// delta that makes a reconnect work where in-session recovery didn't). Idempotent + cheap: when
/// nothing besides `gdi_name` is attached, [`isolate_displays`] finds nothing to detach and commits
/// nothing — so this is safe to call on every throttled recovery tick (no display thrash).
pub(crate) fn reassert_isolation(gdi_name: &str) {
    // Only when sole-display isolation is explicitly opted into (see create()): otherwise re-isolating
    // would itself trigger the independent-flip storm we're avoiding.
    if std::env::var("PUNKTFUNK_ISOLATE_DISPLAYS").is_err() {
        return;
    }
    unsafe {
        let _ = isolate_displays(gdi_name);
    }
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
    isolated: Vec<(String, DEVMODEW)>,
    ccd_saved: Option<SavedConfig>,
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
            while !stop_t.load(Ordering::Relaxed) {
                let mut none: [u8; 0] = [];
                unsafe {
                    let _ = ioctl(h, IOCTL_DRIVER_PING, &[], &mut none);
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
        let isolated: Vec<(String, DEVMODEW)> = Vec::new(); // legacy GDI detach unused (CCD path below)
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
                    tracing::info!("display isolation skipped (PUNKTFUNK_NO_ISOLATE) — IDD stays extended");
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
            isolated,
            ccd_saved,
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
        restore_displays(&self.isolated);
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
        tracing::info!("SudoVDA protocol {}.{}.{} (test={})", ver[0], ver[1], ver[2], ver[3]);
    }
    let mut wd = [0u8; 8];
    g.watchdog_s = if unsafe { ioctl(device, IOCTL_GET_WATCHDOG, &[], &mut wd) }.is_ok() {
        u32::from_le_bytes([wd[0], wd[1], wd[2], wd[3]]).max(1)
    } else {
        3
    };
    tracing::info!("SudoVDA watchdog timeout {}s", g.watchdog_s);
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
        tracing::info!(refs = *refs, "SudoVDA monitor reused (concurrent / reconfigure session)");
        let pm = Some((mon.mode.width, mon.mode.height, mon.mode.refresh_hz));
        let target = mon.target();
        return Ok(VirtualOutput {
            node_id: 0,
            preferred_mode: pm,
            win_capture: target,
            keepalive: Box::new(MonitorLease),
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
    g.state = MgrState::Active { mon, refs: 1 };
    Ok(VirtualOutput {
        node_id: 0,
        preferred_mode: pm,
        win_capture: target,
        keepalive: Box::new(MonitorLease),
    })
}

/// Re-apply a (possibly new) mode to a reused monitor on reconnect, re-resolving its GDI name.
unsafe fn mgr_reconfigure(mon: &mut Monitor, mode: Mode) {
    tracing::info!(
        old = format!("{}x{}@{}", mon.mode.width, mon.mode.height, mon.mode.refresh_hz),
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
fn mgr_release() {
    let mut g = MGR.lock().unwrap();
    g.state = match std::mem::replace(&mut g.state, MgrState::Idle) {
        MgrState::Active { mon, refs } if refs > 1 => MgrState::Active { mon, refs: refs - 1 },
        MgrState::Active { mon, .. } => {
            let ms = linger_ms();
            tracing::info!(linger_ms = ms, "SudoVDA: last session left — lingering before teardown");
            MgrState::Lingering {
                mon,
                until: Instant::now() + Duration::from_millis(ms),
            }
        }
        other => other,
    };
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

/// A session's lease on the shared monitor. Drop releases the refcount (→ linger when it hits 0).
struct MonitorLease;
impl Drop for MonitorLease {
    fn drop(&mut self) {
        mgr_release();
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
