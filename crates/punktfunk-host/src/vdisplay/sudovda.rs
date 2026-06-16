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
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
    DISPLAYCONFIG_PATH_ACTIVE, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS, SDC_ALLOW_CHANGES, SDC_APPLY,
    SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_GLOBAL, CDS_NORESET,
    CDS_SET_PRIMARY, CDS_TEST, CDS_TYPE, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW,
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
const MONITOR_GUID: GUID = GUID::from_u128(0x70756E6B_7466_756E_6B30_000000000001);

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

    let dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        dmFields: DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_BITSPERPEL | DM_POSITION,
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
            // Make it the PRIMARY display: a blank *extended* IDD output isn't composited by the DWM,
            // so it produces no duplication frames. As primary it carries the shell/cursor → frames
            // flow (this is what Apollo does). Position is (0,0) via DM_POSITION (zeroed by default).
            CDS_UPDATEREGISTRY | CDS_GLOBAL | CDS_SET_PRIMARY,
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

/// Detach every display except `keep_gdi_name`, leaving the SudoVDA virtual output as the ONLY
/// display. This is the SudoVDA/Apollo "isolate the virtual display" move and the key to capturing
/// the secure desktop: Windows renders the login / UAC (Winlogon) desktop on the physical/primary
/// display and resets the topology when it switches there — with a physical monitor still attached
/// (e.g. an LG TV), the login lands on it and our virtual output goes perpetually ACCESS_LOST. With
/// the physical detached and the change PERSISTED to the registry, Winlogon reads "only the virtual
/// is attached" and the secure desktop has nowhere to render but the output we capture.
///
/// Returns the displays we detached plus their saved modes so teardown can restore them.
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
        paths.len() as u32,
        Some(paths.as_ptr()),
        modes.len() as u32,
        Some(modes.as_ptr()),
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
        paths.len() as u32,
        Some(paths.as_ptr()),
        modes.len() as u32,
        Some(modes.as_ptr()),
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

/// A live SudoVDA control handle. One per host; `create` adds/removes monitors on it.
pub struct SudoVdaDisplay {
    device: HANDLE,
    watchdog_s: u32,
}

// The HANDLE is a kernel object usable from any thread; we only ever issue serialized IOCTLs.
unsafe impl Send for SudoVdaDisplay {}

impl SudoVdaDisplay {
    pub fn new() -> Result<Self> {
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
        let watchdog_s = if unsafe { ioctl(device, IOCTL_GET_WATCHDOG, &[], &mut wd) }.is_ok() {
            u32::from_le_bytes([wd[0], wd[1], wd[2], wd[3]]).max(1)
        } else {
            3
        };
        tracing::info!("SudoVDA watchdog timeout {watchdog_s}s");
        Ok(Self { device, watchdog_s })
    }
}

impl Drop for SudoVdaDisplay {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.device);
        }
    }
}

impl VirtualDisplay for SudoVdaDisplay {
    fn name(&self) -> &'static str {
        "sudovda"
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        let mut device_name = [0u8; 14];
        let nm = b"punktfunk";
        device_name[..nm.len()].copy_from_slice(nm);
        let add = AddParams {
            width: mode.width,
            height: mode.height,
            refresh: mode.refresh_hz,
            guid: MONITOR_GUID,
            device_name,
            serial: [0u8; 14],
        };
        // Pin the IDD's RENDER GPU to the NVENC/capture GPU (e.g. the 4090) BEFORE adding the target.
        // On a multi-adapter box (SudoVDA IDD + discrete GPU) DXGI otherwise reparents the virtual
        // output onto whichever GPU its hybrid-preference path resolves, which storms ACCESS_LOST
        // (0x887A0026) on the secure/HDR desktop. Apollo's SET_RENDER_ADAPTER fixes this and MUST be
        // issued before ADD. Best-effort: a driver that rejects it just keeps the default render GPU.
        let pinned = unsafe { resolve_render_adapter_luid() };
        if let Some(luid) = pinned {
            match unsafe { set_render_adapter(self.device, luid) } {
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
        unsafe { ioctl(self.device, IOCTL_ADD, add_bytes, &mut out) }.with_context(|| {
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
        let device_raw = self.device.0 as isize;
        let interval = Duration::from_millis(self.watchdog_s as u64 * 1000 / 3);
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
        let mut isolated: Vec<(String, DEVMODEW)> = Vec::new();
        let mut ccd_saved: Option<SavedConfig> = None;
        match &gdi_name {
            Some(n) => {
                tracing::info!("SudoVDA target {} -> {n}", ao.target_id);
                // ADD only advertises the mode; force it active so DXGI captures the requested size.
                set_active_mode(n, mode);
                // Detach every other display so the secure desktop (Winlogon/UAC) renders here too.
                // CCD isolation is the one that works on a hybrid box (the legacy GDI enum misses the
                // iGPU-attached monitor); the legacy pass stays as a no-op fallback.
                isolated = unsafe { isolate_displays(n) };
                ccd_saved = unsafe { isolate_displays_ccd(ao.target_id) };
                thread::sleep(Duration::from_millis(1500)); // let the topology settle before capture opens
            }
            None => tracing::warn!(
                "SudoVDA target {} not yet an active display path (needs a WDDM GPU to activate)",
                ao.target_id
            ),
        }

        Ok(VirtualOutput {
            node_id: 0, // unused on Windows; the capture target is the GDI name below
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            win_capture: gdi_name
                .clone()
                .map(|n| crate::capture::dxgi::WinCaptureTarget {
                    adapter_luid: crate::capture::dxgi::pack_luid(ao.luid),
                    gdi_name: n,
                    // The SudoVDA target id is stable across secure-desktop topology rebuilds; the
                    // GDI name is NOT, so capture re-resolves the name from this on every recovery.
                    target_id: ao.target_id,
                }),
            keepalive: Box::new(SudoVdaKeepalive {
                device: device_raw,
                guid: MONITOR_GUID,
                stop,
                pinger: Some(pinger),
                gdi_name,
                isolated,
                ccd_saved,
            }),
        })
    }
}

/// RAII teardown: stop the ping thread, then REMOVE the monitor by its GUID. Does NOT close the
/// device handle — that belongs to [`SudoVdaDisplay`], which outlives the output.
struct SudoVdaKeepalive {
    device: isize,
    guid: GUID,
    stop: Arc<AtomicBool>,
    pinger: Option<JoinHandle<()>>,
    #[allow(dead_code)] // consumed by the Windows capture backend (not yet wired)
    gdi_name: Option<String>,
    /// Displays detached by [`isolate_displays`] (legacy), restored here on teardown.
    isolated: Vec<(String, DEVMODEW)>,
    /// Active topology saved by [`isolate_displays_ccd`] (the one that works on hybrid boxes),
    /// restored here on teardown.
    ccd_saved: Option<SavedConfig>,
}

impl Drop for SudoVdaKeepalive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.pinger.take() {
            let _ = j.join();
        }
        // Re-attach the physical display(s) we detached BEFORE removing the virtual output, so the
        // box is never left with zero displays. Restore the CCD topology first (the one that actually
        // detached on a hybrid box), then the legacy pass.
        if let Some(saved) = &self.ccd_saved {
            unsafe { restore_displays_ccd(saved) };
        }
        unsafe { restore_displays(&self.isolated) };
        let rp = RemoveParams { guid: self.guid };
        let rp_bytes = unsafe {
            std::slice::from_raw_parts(&rp as *const _ as *const u8, size_of::<RemoveParams>())
        };
        let mut none: [u8; 0] = [];
        let h = HANDLE(self.device as *mut c_void);
        if let Err(e) = unsafe { ioctl(h, IOCTL_REMOVE, rp_bytes, &mut none) } {
            tracing::warn!("SudoVDA REMOVE failed: {e:#}");
        } else {
            tracing::info!("SudoVDA monitor removed");
        }
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
