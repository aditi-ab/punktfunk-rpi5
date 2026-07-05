//! Backend-neutral Windows display utilities — the CCD (QueryDisplayConfig) + GDI helpers shared by the
//! virtual-display backends (pf-vdisplay, SudoVDA) and the capturers (IDD-push, WGC, DDA): GDI-name
//! resolution, advanced-color (HDR) get/set, active-mode set, and CCD topology isolate/restore.
//!
//! These are display-utility, NOT SudoVDA-specific (a pf-vdisplay monitor's target_id is a real OS target
//! id, so they operate identically), so they live here rather than in the SudoVDA backend — breaking the
//! circular reach-in where the capturers + the pf-vdisplay backend reached into `vdisplay::sudovda` for
//! them, which let the SudoVDA backend be dropped without losing them (audit §9 / Goal 2 — done). The
//! plan's `windows/display_ccd.rs`. Extracted verbatim from the former SudoVDA backend before its removal.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use std::mem::size_of;

use windows::core::PCWSTR;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QueryDisplayConfig, SetDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    QDC_ONLY_ACTIVE_PATHS, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_FORCE_MODE_ENUMERATION,
    SDC_SAVE_TO_DATABASE, SDC_TOPOLOGY_EXTEND, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows::Win32::Foundation::POINTL;
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_TEST, CDS_UPDATEREGISTRY, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
};

use crate::vdisplay::Mode;

/// Force the desktop into EXTEND topology - the programmatic equivalent of the Win+P / DisplaySwitch
/// "Extend" shortcut. Windows defaults a FRESHLY-ADDED monitor into CLONE/duplicate mode when a
/// physical display is already active (e.g. a laptop panel): a cloned IddCx output shares the panel's
/// source, so the OS never commits a distinct path for it, never calls ASSIGN_SWAPCHAIN, and capture
/// sees no frames (`resolve_gdi_name` stays `None` and the session fails "not an active display path").
/// Applying the EXTEND preset across the live set of connected displays makes the new IddCx monitor its
/// OWN active path, so the rest of bring-up (`resolve_gdi_name` -> `set_active_mode` ->
/// `isolate_displays_ccd`) proceeds. Best-effort + idempotent: a no-op on a single-display (already
/// sole/extended) box, so it is safe to call unconditionally. `rc == 0` is success.
pub(crate) unsafe fn force_extend_topology() {
    // A topology flag with no supplied path/mode arrays tells the OS to recompute + apply that preset
    // for the currently-connected displays (the same code path DisplaySwitch.exe drives).
    let rc = SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND);
    if rc == 0 {
        tracing::info!(
            "display topology forced to EXTEND (a new IddCx monitor would otherwise be CLONED onto the \
             existing panel -> no distinct source -> no frames)"
        );
    } else {
        tracing::warn!("display force-EXTEND topology: SetDisplayConfig rc={rc:#x}");
    }
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

/// The virtual display's CURRENT active resolution `(width, height)` via the GDI/CCD API, or `None` if the
/// target isn't an active display yet / the query fails. The IDD-push capturer sizes its ring to this
/// ACTUAL mode and polls it to recreate the ring when it changes — a fullscreen game can change the
/// virtual display's mode out from under the session-negotiated one (game-capture bug GB1).
///
/// # Safety
/// Calls the GDI/CCD APIs; safe to call from any thread.
pub(crate) unsafe fn active_resolution(target_id: u32) -> Option<(u32, u32)> {
    let gdi = resolve_gdi_name(target_id)?;
    let wname: Vec<u16> = gdi.encode_utf16().chain(std::iter::once(0)).collect();
    let mut dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let ok = EnumDisplaySettingsW(PCWSTR(wname.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool();
    if !ok || dm.dmPelsWidth == 0 || dm.dmPelsHeight == 0 {
        return None;
    }
    Some((dm.dmPelsWidth, dm.dmPelsHeight))
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
// pub(crate) so vdisplay::pf_vdisplay can reuse this backend-neutral CCD/GDI mode-set helper
// (a pf-vdisplay monitor's GDI name is a real OS device name, so it works unchanged).
pub(crate) fn set_active_mode(gdi_name: &str, mode: Mode) {
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
        // SAFETY: `wname` is a live NUL-terminated UTF-16 device name (built above) whose pointer stays
        // valid for the call; `&mut dm` is a live DEVMODEW with `dmSize` set that EnumDisplaySettingsW
        // fills in for mode index `i`. Both outlive this synchronous call; the API only reads the name
        // and writes `dm`, so nothing aliases.
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
    // SAFETY: `wname` is a live NUL-terminated UTF-16 device name and `&dm` is a live DEVMODEW describing
    // the requested mode; both outlive the call. CDS_TEST only validates the mode (no apply), the two
    // trailing args are null, and the API only reads its inputs.
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
    // SAFETY: same inputs as the CDS_TEST call above — `wname` (live NUL-terminated device name) and
    // `&dm` (live DEVMODEW) both outlive the call; CDS_UPDATEREGISTRY applies the already-validated mode,
    // and the API only reads its inputs.
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
// pub(crate) so vdisplay::pf_vdisplay's Monitor can hold the same saved-topology type.
pub(crate) type SavedConfig = (Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>);

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
// pub(crate) so vdisplay::pf_vdisplay can reuse this backend-neutral CCD isolation helper
// (it operates on a real OS target id — a pf-vdisplay monitor's target_id qualifies).
pub(crate) unsafe fn isolate_displays_ccd(keep_target_id: u32) -> Option<SavedConfig> {
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

/// **Primary (topology=primary)** — make the virtual output the PRIMARY display while KEEPING every
/// other display ACTIVE (unlike [`isolate_displays_ccd`], which deactivates them). Windows treats the
/// display whose source sits at the desktop origin `(0,0)` as primary, so we move the virtual's source
/// to `(0,0)` and shift every other active source to its right — all paths stay active. Done as ONE
/// atomic CCD `SetDisplayConfig` (NOT GDI `CDS_SET_PRIMARY`, which storms
/// `DXGI_ERROR_MODE_CHANGE_IN_PROGRESS` when another display is live — see [`set_active_mode`]).
/// Returns the original config to restore on teardown.
pub(crate) unsafe fn set_virtual_primary_ccd(keep_target_id: u32) -> Option<SavedConfig> {
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

    // The virtual output's source width, to shift the physicals past it.
    let virt_width = paths.iter().find_map(|p| {
        if p.targetInfo.id != keep_target_id {
            return None;
        }
        let idx = p.sourceInfo.modeInfoIdx as usize;
        let m = modes.get(idx)?;
        (m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)
            .then(|| m.Anonymous.sourceMode.width as i32)
    })?;

    // Reposition each active path's SOURCE once: the virtual to (0,0) (= primary), the rest shifted
    // right by the virtual's width (kept active, no overlap). Dedup source-mode indices (a cloned
    // group would share one).
    let mut done = std::collections::HashSet::new();
    for p in paths.iter() {
        let idx = p.sourceInfo.modeInfoIdx as usize;
        if !done.insert(idx) {
            continue;
        }
        let Some(m) = modes.get_mut(idx) else {
            continue;
        };
        if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            continue;
        }
        if p.targetInfo.id == keep_target_id {
            m.Anonymous.sourceMode.position = POINTL { x: 0, y: 0 };
        } else {
            m.Anonymous.sourceMode.position.x += virt_width;
        }
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
        tracing::info!("display primary (CCD): virtual target {keep_target_id} set PRIMARY at (0,0); physical display(s) kept ACTIVE, shifted right by {virt_width}px");
    } else {
        tracing::warn!("display primary (CCD): SetDisplayConfig failed rc={rc:#x} (virtual {keep_target_id} primary, physicals kept)");
    }
    Some(saved)
}

/// Restore the topology saved by [`isolate_displays_ccd`] (teardown, before the virtual output is
/// removed), re-activating the displays we deactivated.
// pub(crate) so vdisplay::pf_vdisplay can reuse this backend-neutral CCD restore helper.
pub(crate) unsafe fn restore_displays_ccd(saved: &SavedConfig) {
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
