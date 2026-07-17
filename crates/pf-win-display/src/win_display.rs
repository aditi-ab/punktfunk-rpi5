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
// These CCD/GDI FFI helpers were `pub(crate) unsafe fn` before the pf-win-display carve (plan §W6),
// where `missing_safety_doc` stays silent; crossing the crate boundary makes them `pub` and would
// demand a `# Safety` heading on each. Their callers' obligations (call on the right desktop thread
// with a live OS target id) are stated in each fn's prose doc, and this is an internal
// (publish=false) leaf — keep the pre-carve behavior rather than adding 12 formal headings.
#![allow(clippy::missing_safety_doc)]

use std::mem::size_of;

use windows::core::PCWSTR;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QueryDisplayConfig, SetDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPONENT_VIDEO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPOSITE_VIDEO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDI, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDTVDONGLE,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SVIDEO, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY, QDC_ALL_PATHS,
    QDC_ONLY_ACTIVE_PATHS, SDC_ALLOW_CHANGES, SDC_APPLY, SDC_FORCE_MODE_ENUMERATION,
    SDC_SAVE_TO_DATABASE, SDC_TOPOLOGY_EXTEND, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows::Win32::Foundation::POINTL;
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_TEST, CDS_UPDATEREGISTRY, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
};

use punktfunk_core::Mode;

/// Force the desktop into EXTEND topology - the programmatic equivalent of the Win+P / DisplaySwitch
/// "Extend" shortcut. Windows defaults a FRESHLY-ADDED monitor into CLONE/duplicate mode when a
/// physical display is already active (e.g. a laptop panel): a cloned IddCx output shares the panel's
/// source, so the OS never commits a distinct path for it, never calls ASSIGN_SWAPCHAIN, and capture
/// sees no frames (`resolve_gdi_name` stays `None` and the session fails "not an active display path").
/// Applying the EXTEND preset across the live set of connected displays makes the new IddCx monitor its
/// OWN active path, so the rest of bring-up (`resolve_gdi_name` -> `set_active_mode` ->
/// `isolate_displays_ccd`) proceeds. Best-effort + idempotent: a no-op on a single-display (already
/// sole/extended) box, so it is safe to call unconditionally. `rc == 0` is success.
pub unsafe fn force_extend_topology() {
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

/// EXPLICITLY activate `target_id` into its own display path — the last-resort fallback when neither
/// the OS auto-activate nor the EXTEND topology preset lights a freshly-ADDed IDD target. Observed on
/// a lid-closed laptop (field report, Intel iGPU): the clamshell lid policy makes Windows skip the
/// new-monitor auto-activation AND the `SDC_TOPOLOGY_EXTEND` preset returns success without ever
/// committing a path for the IDD, so the target sits connected-but-inactive for the whole retry
/// budget (RDP/Parsec don't need a new console display path, which is why they still work there).
///
/// This is the supplied-config apply Windows' own display Settings uses to turn a monitor on: query
/// ALL paths, keep every currently-active path verbatim, and append the target's inactive path with a
/// source not already driving another display — mode indices invalidated so `SDC_ALLOW_CHANGES` lets
/// the OS pick modes for the new path. Returns `true` when the apply reports success; the caller
/// still re-polls [`resolve_gdi_name`] to confirm the path actually committed.
pub unsafe fn activate_target_path(target_id: u32) -> bool {
    let mut np = 0u32;
    let mut nm = 0u32;
    if GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut np, &mut nm).is_err() {
        return false;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    if QueryDisplayConfig(
        QDC_ALL_PATHS,
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
    paths.truncate(np as usize);
    modes.truncate(nm as usize);

    // Keep the currently-active paths verbatim — their mode indices stay valid because the queried
    // modes array is passed through unchanged.
    let mut supplied: Vec<DISPLAYCONFIG_PATH_INFO> = paths
        .iter()
        .filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0)
        .copied()
        .collect();
    if supplied.iter().any(|p| p.targetInfo.id == target_id) {
        return true; // already active — we raced the OS auto-activate
    }

    // Pick an inactive path for our target whose SOURCE isn't already driving an active display on
    // the same adapter (sharing one would make the IDD a clone — exactly the no-frames state this
    // fallback exists to break out of).
    let Some(cand) = paths.iter().find(|p| {
        p.targetInfo.id == target_id
            && p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0
            && !supplied.iter().any(|a| {
                (
                    a.sourceInfo.adapterId.LowPart,
                    a.sourceInfo.adapterId.HighPart,
                    a.sourceInfo.id,
                ) == (
                    p.sourceInfo.adapterId.LowPart,
                    p.sourceInfo.adapterId.HighPart,
                    p.sourceInfo.id,
                )
            })
    }) else {
        tracing::warn!(
            target_id,
            "explicit path activation: no inactive path with a free source for this target"
        );
        return false;
    };
    let mut new_path = *cand;
    new_path.flags |= DISPLAYCONFIG_PATH_ACTIVE;
    new_path.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    new_path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    supplied.push(new_path);

    // SAVE_TO_DATABASE so Windows remembers the arrangement — the next same-identity ADD (the driver
    // reuses the slot's EDID serial/ConnectorIndex) then auto-activates from the persistence DB and
    // skips this whole fallback ladder.
    let rc = SetDisplayConfig(
        Some(supplied.as_slice()),
        Some(modes.as_slice()),
        SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE,
    );
    if rc == 0 {
        tracing::info!(
            target_id,
            "explicit path activation: supplied-config apply succeeded (target committed alongside {} active path(s))",
            supplied.len() - 1
        );
        true
    } else {
        tracing::warn!(
            target_id,
            "explicit path activation: SetDisplayConfig rc={rc:#x}"
        );
        false
    }
}

/// Resolve the `\\.\DisplayN` GDI name for a virtual-display target id via the CCD API. Returns `None`
/// until the OS activates the target into the desktop topology (needs a real WDDM GPU; on a
/// GPU-less box this stays `None` even though ADD succeeded).
pub unsafe fn resolve_gdi_name(target_id: u32) -> Option<String> {
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
pub unsafe fn active_resolution(target_id: u32) -> Option<(u32, u32)> {
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

/// Toggle the virtual-display target's advanced-color (HDR) state via the CCD API. Disabling HDR while on the
/// secure (Winlogon) desktop makes it render SDR/composed so DXGI Desktop Duplication can capture it
/// (the HDR fullscreen independent-flip otherwise storms `ACCESS_LOST` → black); re-enable on return so
/// WGC keeps HDR on the normal desktop. Returns true on a successful `DisplayConfigSetDeviceInfo`.
pub unsafe fn set_advanced_color(target_id: u32, enable: bool) -> bool {
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
            tracing::debug!(
                target_id,
                enable,
                rc,
                "virtual-display set advanced-color (HDR) state"
            );
            return rc == 0;
        }
    }
    tracing::warn!(
        target_id,
        "virtual-display advanced-color: target not in active paths"
    );
    false
}

/// Read the virtual-display target's CURRENT advanced-color (HDR) state via the CCD API — i.e. whether HDR is
/// actually ON for the virtual display right now (e.g. because the user toggled it in Windows display
/// settings). The capture/encode pipeline follows the monitor's real colorspace (WGC → FP16 → NVENC
/// Main10 BT.2020 PQ), so this is the authoritative "is this an HDR session" signal — NOT the
/// handshake-negotiated bit depth. `None` when the query fails or the target isn't in the active-path
/// list (both happen transiently during a display-topology re-probe): the caller decides the fallback —
/// the capture loop's poller keeps the last known value, since reading a blip as "HDR off" used to cost
/// an HDR session TWO spurious ring recreates (false, then true again a poll later).
pub unsafe fn advanced_color_enabled(target_id: u32) -> Option<bool> {
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
            let mut info = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
            info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO;
            info.header.size = size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32;
            info.header.adapterId = p.targetInfo.adapterId;
            info.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut info.header) == 0 {
                // value bit 1 = advancedColorEnabled (bit 0 = advancedColorSupported).
                return Some((info.Anonymous.value & 0x2) != 0);
            }
            return None;
        }
    }
    None
}

/// Force the freshly-added virtual monitor to the client's exact `WxH@Hz`. The ADD IOCTL only
/// ADVERTISES the mode; Windows otherwise activates an IDD target at a 1280x720 default, so the
/// ACTIVE mode (what DXGI Desktop Duplication captures) must be set explicitly. CDS_TEST first so a
/// mode the driver didn't advertise just leaves the default instead of erroring the session.
// pub so vdisplay::pf_vdisplay can reuse this backend-neutral CCD/GDI mode-set helper
// (a pf-vdisplay monitor's GDI name is a real OS device name, so it works unchanged).
pub fn set_active_mode(gdi_name: &str, mode: Mode) {
    let wname: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();

    // Enumerate the modes the driver actually advertises for this output and pick the best match for
    // the requested RESOLUTION: the exact refresh if present, else the highest advertised refresh
    // <= requested, else the highest available at that resolution. The pf-vdisplay ADD IOCTL advertises
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
// pub so vdisplay::pf_vdisplay's Monitor can hold the same saved-topology type.
pub type SavedConfig = (Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>);

/// `DISPLAYCONFIG_PATH_ACTIVE` (wingdi.h) — the `flags` bit marking a path active. The `windows` crate
/// doesn't export it, so define it here.
const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;

/// `DISPLAYCONFIG_PATH_MODE_IDX_INVALID` (wingdi.h) — "no mode pinned" for a path's source/target
/// mode index; with `SDC_ALLOW_CHANGES` the OS picks the modes itself. Not exported by the `windows`
/// crate either.
const DISPLAYCONFIG_PATH_MODE_IDX_INVALID: u32 = 0xffff_ffff;

/// Query the current ACTIVE display config (paths + modes), truncated to the real counts. `None` on
/// API failure. Shared by [`isolate_displays_ccd`] (snapshot + per-attempt re-query) and
/// [`count_other_active`].
unsafe fn query_active_config() -> Option<SavedConfig> {
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
    Some((paths, modes))
}

/// Count currently-ACTIVE display paths whose target id is not in `keep_target_ids` — i.e. displays
/// that would still be lit besides the managed virtual set. `None` on query failure. Used to VERIFY
/// isolation actually took, and (in the `primary` topology) to detect a physical that is ALREADY
/// active so we can skip a force-EXTEND that would reset its refresh.
pub unsafe fn count_other_active(keep_target_ids: &[u32]) -> Option<u32> {
    let (paths, _) = query_active_config()?;
    Some(
        paths
            .iter()
            .filter(|p| {
                !keep_target_ids.contains(&p.targetInfo.id)
                    && p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
            })
            .count() as u32,
    )
}

/// One CONNECTED display target from a full (`QDC_ALL_PATHS`) CCD sweep — the disturbance-
/// attribution inventory. `external_physical` is the load-bearing bit: a standby TV/monitor on a
/// real connector is the prime suspect for the periodic link-probe stutter class, while internal
/// panels and indirect/virtual targets (our own IDD included) are not.
pub struct TargetInventory {
    pub target_id: u32,
    /// Whether any active path drives this target (part of the desktop right now).
    pub active: bool,
    /// External physical connector (HDMI/DP/DVI/…): candidate for standby link-probe churn.
    pub external_physical: bool,
    /// Short connector label for logs (`"HDMI"`, `"DisplayPort"`, `"internal-panel"`, …).
    pub tech: &'static str,
    /// The monitor's friendly name (`"LG TV SSCR2"`); empty when the EDID carries none.
    pub friendly: String,
    /// Monitor device interface path — maps to the PnP instance id (`monitor_devnode`).
    pub monitor_device_path: String,
}

/// Classify a CCD output technology: `(external physical?, log label)`. Allowlist, not blocklist:
/// new/unknown/indirect technologies read as non-external, so a co-installed third-party virtual
/// display can never be mistaken for a physical suspect (same precision rule as `monitor_devnode`).
fn output_tech_class(tech: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> (bool, &'static str) {
    match tech {
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI => (true, "HDMI"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL => (true, "DisplayPort"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI => (true, "DVI"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15 => (true, "VGA"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL => (true, "UDI"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDI => (true, "SDI"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPONENT_VIDEO => (true, "component"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPOSITE_VIDEO => (true, "composite"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SVIDEO => (true, "S-Video"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDTVDONGLE => (true, "TV-dongle"),
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
        | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS
        | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
        | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED => (false, "internal-panel"),
        _ => (false, "virtual/other"),
    }
}

fn utf16z_str(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Sweep EVERY connected display target (`QDC_ALL_PATHS`, deduped from the source×target path
/// matrix to unique targets) with its name, connector class and active state. Read-only CCD; can
/// briefly serialize on the display-config lock during topology churn — callers must keep it OFF
/// the capture thread (`display_events` runs it on its own listener thread and caches).
pub unsafe fn target_inventory() -> Vec<TargetInventory> {
    let mut np = 0u32;
    let mut nm = 0u32;
    if GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut np, &mut nm).is_err() {
        return Vec::new();
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    if QueryDisplayConfig(
        QDC_ALL_PATHS,
        &mut np,
        paths.as_mut_ptr(),
        &mut nm,
        modes.as_mut_ptr(),
        None,
    )
    .is_err()
    {
        return Vec::new();
    }
    paths.truncate(np as usize);
    // Targets driven by an ACTIVE path. `(LUID parts, target id)` keys: target ids are only
    // unique per adapter.
    let active: Vec<(u32, i32, u32)> = paths
        .iter()
        .filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0)
        .map(|p| {
            (
                p.targetInfo.adapterId.LowPart,
                p.targetInfo.adapterId.HighPart,
                p.targetInfo.id,
            )
        })
        .collect();
    let mut seen: Vec<(u32, i32, u32)> = Vec::new();
    let mut out = Vec::new();
    for p in &paths {
        let t = &p.targetInfo;
        let key = (t.adapterId.LowPart, t.adapterId.HighPart, t.id);
        // `targetAvailable` == a monitor is connected; an ACTIVE target is included regardless
        // (the flag reads FALSE transiently right after a removal).
        if (!t.targetAvailable.as_bool() && !active.contains(&key)) || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let mut req = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        req.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        req.header.adapterId = t.adapterId;
        req.header.id = t.id;
        // `req` is a properly-sized DISPLAYCONFIG_TARGET_DEVICE_NAME local whose header
        // (type/size/adapterId/id) is fully initialised; the API writes only within the struct.
        if DisplayConfigGetDeviceInfo(&mut req.header) != 0 {
            continue; // target with no queryable monitor — nothing to attribute to
        }
        let (external_physical, tech) = output_tech_class(req.outputTechnology);
        out.push(TargetInventory {
            target_id: t.id,
            active: active.contains(&key),
            external_physical,
            tech,
            friendly: utf16z_str(&req.monitorFriendlyDeviceName),
            monitor_device_path: utf16z_str(&req.monitorDevicePath),
        });
    }
    out
}

/// Robust display isolation via the CCD API. The naive GDI approach (EnumDisplayDevices +
/// ChangeDisplaySettings) MISSES displays on a hybrid box — an iGPU-attached physical monitor isn't
/// flagged `ATTACHED_TO_DESKTOP` in the GDI enum, so it's never detached and the secure desktop /
/// lock screen lands on IT while our virtual output freezes. `QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)`
/// sees every active path; we deactivate all of them EXCEPT the managed virtual target **set**
/// (`design/display-management.md` §6.1: "exclusive" means the managed set stays active — with
/// parallel displays a sibling slot is never deactivated), leaving the virtual display(s) as the sole
/// desktop so ALL content (incl. Winlogon) renders to them. Apollo isolates the same way (CCD).
/// Re-issued with the grown/shrunk set on each slot add/remove while the group lives; the FIRST call's
/// returned config is what teardown restores (the caller keeps it on the group record and discards
/// later returns). Returns the original active config to restore on teardown.
// pub so vdisplay::pf_vdisplay can reuse this backend-neutral CCD isolation helper
// (it operates on real OS target ids — a pf-vdisplay monitor's target_id qualifies).
pub unsafe fn isolate_displays_ccd(keep_target_ids: &[u32]) -> Option<SavedConfig> {
    // Snapshot the ORIGINAL active config ONCE for restore-on-teardown, before any changes.
    let saved = query_active_config()?;

    // Deactivate every non-keep display, then VERIFY and RETRY. A field-reported bug had a physical
    // monitor STAY ACTIVE in exclusive mode, so we don't trust a single SetDisplayConfig: re-query the
    // live topology each attempt and re-apply until ONLY the keep set is active. Secure-desktop
    // correctness depends on this — the lock screen must not land on a stray panel while we stream.
    for attempt in 1..=4u32 {
        let (mut paths, modes) = query_active_config()?;
        let mut others = 0u32;
        for p in paths.iter_mut() {
            if keep_target_ids.contains(&p.targetInfo.id) {
                continue;
            }
            if p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 {
                p.flags &= !DISPLAYCONFIG_PATH_ACTIVE; // mark this path inactive
                others += 1;
            }
        }
        // Commit the config. Even when nothing needed deactivating we re-commit: a legacy mode-set does
        // NOT drive the IddCx adapter's EVT_IDD_CX_ADAPTER_COMMIT_MODES, and without COMMIT_MODES the OS
        // never calls ASSIGN_SWAPCHAIN, so the driver receives no frames. SDC_FORCE_MODE_ENUMERATION
        // forces the re-commit; SAVE_TO_DATABASE only in the sole-path case (matches prior behavior —
        // don't permanently rewrite the user's multi-display layout; the teardown restore handles it).
        let mut flags = SDC_APPLY
            | SDC_USE_SUPPLIED_DISPLAY_CONFIG
            | SDC_ALLOW_CHANGES
            | SDC_FORCE_MODE_ENUMERATION;
        if others == 0 {
            flags |= SDC_SAVE_TO_DATABASE;
        }
        let rc = SetDisplayConfig(Some(paths.as_slice()), Some(modes.as_slice()), flags);

        // VERIFY the OUTCOME (rc alone lies — a "successful" apply can leave a panel active): re-query
        // and confirm no non-keep display survived. Only then is the virtual set truly the sole desktop.
        let survivors = count_other_active(keep_target_ids).unwrap_or(0);
        if survivors == 0 {
            tracing::info!("display isolate (CCD): target set {keep_target_ids:?} is the SOLE active desktop (attempt {attempt}/4, deactivated {others}, rc={rc:#x})");
            return Some(saved);
        }
        tracing::warn!("display isolate (CCD): {survivors} display(s) STILL active after attempt {attempt}/4 (deactivated {others}, rc={rc:#x}) — re-querying + retrying");
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    tracing::error!("display isolate (CCD): failed to isolate target set {keep_target_ids:?} after 4 attempts — a non-virtual display stayed active (field-reported exclusive-mode bug)");
    Some(saved)
}

/// The desktop-space rectangle `(x, y, w, h)` of `target_id`'s SOURCE — where this display's
/// region lives in the desktop coordinate space. `None` while the target isn't an active path.
/// Used by the IDD-push compose kick to dirty THE TARGET display: with parallel displays the
/// cursor sits on ONE of them, and a cursor wiggle only dirties that one — a sibling display's
/// kick must first know where to send the cursor (Stage W3 on-glass finding).
pub unsafe fn source_desktop_rect(target_id: u32) -> Option<(i32, i32, i32, i32)> {
    let (paths, modes) = query_active_config()?;
    for p in &paths {
        if p.targetInfo.id != target_id || p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        let idx = p.sourceInfo.Anonymous.modeInfoIdx as usize;
        let m = modes.get(idx)?;
        if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            return None;
        }
        let sm = m.Anonymous.sourceMode;
        return Some((
            sm.position.x,
            sm.position.y,
            sm.width as i32,
            sm.height as i32,
        ));
    }
    None
}

/// Place each managed virtual target's SOURCE at the given desktop-space origin, as ONE atomic CCD
/// `SetDisplayConfig` (design `display-management.md` §6.2 — the Windows arm of the pure
/// `vdisplay/layout.rs` arrangement; positions come from `arrange`, this only commits them). Windows
/// treats the source at `(0,0)` as primary, so auto-row's first member lands primary — the group's
/// designated member. Paths not named stay where they are. Best-effort: a failure leaves the OS
/// placement (mouse crossing may not match the layout table until the next apply).
pub unsafe fn apply_source_positions(positions: &[(u32, i32, i32)]) {
    if positions.len() < 2 {
        return; // a single (or no) member sits at the origin — nothing to arrange
    }
    let Some((paths, mut modes)) = query_active_config() else {
        return;
    };
    // Dedup source-mode indices (a cloned group shares one) — same discipline as
    // `set_virtual_primary_ccd`.
    let mut done = std::collections::HashSet::new();
    let mut moved = 0u32;
    for p in paths.iter() {
        let Some(&(_, x, y)) = positions.iter().find(|(t, _, _)| *t == p.targetInfo.id) else {
            continue;
        };
        let idx = p.sourceInfo.Anonymous.modeInfoIdx as usize;
        if !done.insert(idx) {
            continue;
        }
        let Some(m) = modes.get_mut(idx) else {
            continue;
        };
        if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            continue;
        }
        m.Anonymous.sourceMode.position = POINTL { x, y };
        moved += 1;
    }
    if moved == 0 {
        return;
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
        tracing::info!(
            ?positions,
            "display layout (CCD): group source origins applied"
        );
    } else {
        tracing::warn!(
            ?positions,
            "display layout (CCD): SetDisplayConfig rc={rc:#x}"
        );
    }
}

/// **Primary (topology=primary)** — make the virtual output the PRIMARY display while KEEPING every
/// other display ACTIVE (unlike [`isolate_displays_ccd`], which deactivates them). Windows treats the
/// display whose source sits at the desktop origin `(0,0)` as primary, so we move the virtual's source
/// to `(0,0)` and shift every other active source to its right — all paths stay active. Done as ONE
/// atomic CCD `SetDisplayConfig` (NOT GDI `CDS_SET_PRIMARY`, which storms
/// `DXGI_ERROR_MODE_CHANGE_IN_PROGRESS` when another display is live — see [`set_active_mode`]).
/// Returns the original config to restore on teardown.
pub unsafe fn set_virtual_primary_ccd(keep_target_id: u32) -> Option<SavedConfig> {
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

    // The virtual output's source width, to lay the other displays out to its right.
    let virt_width = paths.iter().find_map(|p| {
        if p.targetInfo.id != keep_target_id {
            return None;
        }
        let idx = p.sourceInfo.Anonymous.modeInfoIdx as usize;
        let m = modes.get(idx)?;
        // `then_some` (eager): `sourceMode.width` is a POD `u32` union read, discarded when the arm is
        // false — no lazy guard needed. (`then(|| …)` here trips clippy::unnecessary_lazy_evaluations.)
        (m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)
            .then_some(m.Anonymous.sourceMode.width as i32)
    })?;
    let others = paths.len().saturating_sub(1);

    // Reposition each active path's SOURCE once: the virtual to (0,0) (= primary), the other
    // displays PACKED left-to-right from the virtual's right edge — kept active, no overlap and no
    // gap (vs. blindly shifting each by virt_width, which leaves a dead gap when EXTEND already
    // placed them to the right). Dedup source-mode indices (a cloned group shares one).
    let mut next_x = virt_width;
    let mut done = std::collections::HashSet::new();
    for p in paths.iter() {
        let idx = p.sourceInfo.Anonymous.modeInfoIdx as usize;
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
            let w = m.Anonymous.sourceMode.width as i32;
            m.Anonymous.sourceMode.position = POINTL { x: next_x, y: 0 };
            next_x += w;
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
        tracing::info!("display primary (CCD): virtual target {keep_target_id} set PRIMARY at (0,0); {others} other display(s) kept ACTIVE + packed to its right");
    } else {
        tracing::warn!("display primary (CCD): SetDisplayConfig failed rc={rc:#x} (virtual {keep_target_id} primary, physicals kept)");
    }
    Some(saved)
}

/// Restore the topology saved by [`isolate_displays_ccd`] (teardown, before the virtual output is
/// removed), re-activating the displays we deactivated.
// pub so vdisplay::pf_vdisplay can reuse this backend-neutral CCD restore helper.
pub unsafe fn restore_displays_ccd(saved: &SavedConfig) {
    let (paths, modes) = saved;
    if paths.is_empty() {
        return;
    }
    let rc = SetDisplayConfig(
        Some(paths.as_slice()),
        Some(modes.as_slice()),
        SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
    );
    if rc == 0 {
        tracing::info!("display isolate (CCD): restored original topology");
    } else {
        tracing::warn!("display isolate (CCD): topology restore failed rc={rc:#x} — physical displays may be left deactivated");
    }
}
