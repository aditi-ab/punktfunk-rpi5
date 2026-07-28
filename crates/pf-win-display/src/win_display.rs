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
// …and that program only covers a whole `unsafe fn` body once the body needs its own block: in
// edition 2021 `unsafe_op_in_unsafe_fn` is allow-by-default, which exempted every CCD/GDI helper
// below — including `restore_displays_ccd`, the call pf-vdisplay's teardown path depends on to give
// the operator their physical panels back.
#![deny(unsafe_op_in_unsafe_fn)]
// The CCD/GDI helpers below are SAFE fns. They were `unsafe fn` for a decade of habit rather than a
// memory-safety obligation: every one takes `Copy` scalars or borrowed Rust data, returns owned
// values, and discharges its own FFI preconditions internally (`retry_set_display_config` even binds
// the input desktop itself). What their `# Safety` sections actually described — "call under the
// manager `state` lock" — is a SERIALIZATION requirement: calling one unlocked races the topology
// mutator and yields a stale answer, which is a correctness bug, not undefined behaviour.
//
// Encoding that with `unsafe fn` cost ~55 `unsafe` blocks across three crates whose proofs could
// only restate "this takes a u32 and returns a String", which is how `unsafe` stopped meaning
// anything here. The requirement is now prose on the functions that have it, and `unsafe` marks
// only the FFI calls inside.

// THE CCD CONTRACT, stated once — most `// SAFETY:` proofs below are an instance of it.
//
// `GetDisplayConfigBufferSizes(flags, &mut np, &mut nm)` writes the counts the OS wants for the
// given flags. `QueryDisplayConfig(flags, &mut np, paths, &mut nm, modes, None)` then fills those
// two buffers and writes back the counts it actually used, which are <= the ones asked for. Every
// call site here allocates `paths`/`modes` with EXACTLY `np`/`nm` elements immediately after the
// sizing call and passes `as_mut_ptr()` alongside the same `&mut np`/`&mut nm` it sized from — so
// the pointer and its count can never disagree, and the OS cannot write past either buffer. The
// `Vec`s are locals that outlive the call; the API retains neither pointer (both are synchronous,
// and the trailing `None` is the optional topology-id out-param). The `.truncate(np)/.truncate(nm)`
// that usually follows is a correctness step, not a safety one.
//
// Two obligations the compiler cannot see, and every caller here meets:
//   * These are FFI, so a torn `DISPLAYCONFIG_*` array would be UB — but the arrays only ever come
//     from `QueryDisplayConfig` itself, or from a `SavedConfig` this module produced earlier.
//   * `SetDisplayConfig` writes global OS state. The callers serialise it under pf-vdisplay's
//     manager `state` lock (each fn's prose doc says so), and `input_desktop::retry_set_display_config`
//     binds it to the input desktop, which is the one precondition a caller could otherwise get wrong.
//
// UNION READS, also stated once. Two `DISPLAYCONFIG` unions are read below and they justify
// differently:
//   * `sourceInfo/targetInfo.Anonymous.modeInfoIdx` (and `…_ADVANCED_COLOR_INFO.Anonymous.value`)
//     overlay a `u32` with a same-sized bitfield struct. BOTH variants are POD with no invalid bit
//     patterns, so the read is well-defined whichever one the OS wrote — there is no discriminant to
//     get wrong. The `modeInfoIdx` it yields is then bounds-checked with `modes.get(idx)`, which is a
//     correctness step, not a safety one.
//   * `DISPLAYCONFIG_MODE_INFO.Anonymous.sourceMode` IS discriminated — by the sibling `infoType`
//     field — so every read of it below is guarded by `infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE`
//     on the same `DISPLAYCONFIG_MODE_INFO`. That guard is the proof; if one ever moves away from its
//     read, the union access stops being justified even though it still compiles.
use std::mem::size_of;

use windows::core::PCWSTR;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QueryDisplayConfig, SetDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPONENT_VIDEO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPOSITE_VIDEO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDI, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDTVDONGLE,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SVIDEO, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY, QDC_ALL_PATHS, QDC_ONLY_ACTIVE_PATHS, SDC_ALLOW_CHANGES,
    SDC_APPLY, SDC_FORCE_MODE_ENUMERATION, SDC_SAVE_TO_DATABASE, SDC_TOPOLOGY_EXTEND,
    SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows::Win32::Foundation::POINTL;
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_RESET, CDS_TEST, CDS_UPDATEREGISTRY,
    DEVMODEW, DISP_CHANGE_FAILED, DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
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
pub fn force_extend_topology() {
    // A topology flag with no supplied path/mode arrays tells the OS to recompute + apply that preset
    // for the currently-connected displays (the same code path DisplaySwitch.exe drives).
    let rc = crate::input_desktop::retry_set_display_config(|| {
        // SAFETY: no arrays are supplied — the two `None`s tell the OS to recompute the preset
        // itself, so there is no buffer/count pair to get wrong. Bound to the input desktop by
        // `retry_set_display_config`, which is this call's one caller-side obligation.
        unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND) }
    });
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
pub fn activate_target_path(target_id: u32) -> bool {
    let mut np = 0u32;
    let mut nm = 0u32;
    // SAFETY: the CCD contract at the top of this file — `&mut np`/`&mut nm` are live
    // locals the OS fills with the counts it wants for these flags.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut np, &mut nm) }.is_err() {
        return false;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    // SAFETY: the CCD contract — `paths`/`modes` were just allocated with exactly `np`/`nm`
    // elements from the sizing call above, and are handed over with those same counts.
    if unsafe {
        QueryDisplayConfig(
            QDC_ALL_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        )
    }
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
    // SAFETY: the CCD contract at the top of this file — the path/mode arrays go over as
    // slices, so pointer and length cannot disagree, and both outlive this synchronous
    // call. `retry_set_display_config` binds it to the input desktop, which is the one
    // precondition a caller of this global-state write could otherwise get wrong.
    let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
        SetDisplayConfig(
            Some(supplied.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE,
        )
    });
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
pub fn resolve_gdi_name(target_id: u32) -> Option<String> {
    let mut np = 0u32;
    let mut nm = 0u32;
    // SAFETY: the CCD contract at the top of this file — `&mut np`/`&mut nm` are live
    // locals the OS fills with the counts it wants for these flags.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) }.is_err() {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    // SAFETY: the CCD contract — `paths`/`modes` were just allocated with exactly `np`/`nm`
    // elements from the sizing call above, and are handed over with those same counts.
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        )
    }
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
            // SAFETY: `src.header` is a live local whose `size` field was just set to the
            // enclosing struct's own `size_of`, which is the contract that tells the OS how many bytes
            // it may touch; the struct outlives this synchronous call.
            if unsafe { DisplayConfigGetDeviceInfo(&mut src.header) } == 0 {
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
/// Safe to call from any thread.
pub fn active_resolution(target_id: u32) -> Option<(u32, u32)> {
    active_mode(target_id).map(|(w, h, _)| (w, h))
}

/// The target's CURRENT active mode as `(width, height, refresh_hz)` — what the OS actually
/// committed, which is not always what was asked for: `set_active_mode` deliberately falls back to
/// the highest advertised refresh <= requested rather than losing the client's resolution. Callers
/// that RECORD a mode must record this, or they claim a refresh the display is not running.
pub fn active_mode(target_id: u32) -> Option<(u32, u32, u32)> {
    let gdi = resolve_gdi_name(target_id)?;
    let wname: Vec<u16> = gdi.encode_utf16().chain(std::iter::once(0)).collect();
    let mut dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `wname` is a live NUL-terminated UTF-16 device name and `&mut dm` a live DEVMODEW
    // out-param with `dmSize` set; the synchronous query only reads the name and fills `dm`.
    let ok =
        unsafe { EnumDisplaySettingsW(PCWSTR(wname.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }
            .as_bool();
    if !ok || dm.dmPelsWidth == 0 || dm.dmPelsHeight == 0 {
        return None;
    }
    Some((dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency))
}

/// Verified-state topology-settle wait (latency plan P0.2): poll the CCD state until the target is
/// actually COMMITTED — an active path exists (the GDI name resolves) and the active resolution
/// equals the requested one — instead of sleeping a fixed interval. The conditions are exactly what
/// `resolve_gdi_name`/`set_active_mode` already established once; this waits until the OS reports
/// them stable. `ceiling` (the old fixed sleep) is the worst-case bound: a mode the driver rejected
/// (`set_active_mode` left the OS default) or a slow third-party CCD-lock holder (SteelSeries
/// class) burns the ceiling and proceeds — behavior identical to the fixed sleep it replaces.
/// Returns `true` when the state verified (typical: one or two 25 ms polls), `false` on ceiling.
///
/// Call under the manager `state` lock like the callers it serve — a *serialization* requirement,
/// not a soundness one: reading topology unlocked races the mutator and yields a stale answer.
pub fn wait_mode_settled(target_id: u32, mode: Mode, ceiling: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + ceiling;
    loop {
        // `&&` short-circuits, so the second CCD query still does not run while the target has no
        // active path at all — this polls every 25 ms. (It was nested only so each call could carry
        // its own `unsafe` proof; both are safe fns now.)
        if resolve_gdi_name(target_id).is_some()
            && active_resolution(target_id) == Some((mode.width, mode.height))
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Re-commit the CURRENT active config with `SDC_FORCE_MODE_ENUMERATION` — the nudge that makes
/// the OS re-query an indirect display's target modes. Observed on-glass (P2): after
/// `IddCxMonitorUpdateModes2` the OS did NOT re-enumerate on its own within 2 s, so a freshly
/// advertised mode never became settable; the isolate/layout paths already re-commit with this
/// flag for the same "the OS won't re-evaluate unless told" class. Best-effort.
///
/// Call under the manager `state` lock — it is the sole topology mutator, and concurrent applies
/// fight each other. A serialization requirement, not a soundness one.
pub fn force_mode_reenumeration() -> bool {
    // SAFETY: `query_active_config` is this module's own CCD helper: it takes nothing and returns owned
    // `Vec`s built from a fresh `QueryDisplayConfig`, so it has no caller obligation at all.
    let Some((paths, modes)) = query_active_config() else {
        return false;
    };
    // SAFETY: the CCD contract at the top of this file — the path/mode arrays go over as
    // slices, so pointer and length cannot disagree, and both outlive this synchronous
    // call. `retry_set_display_config` binds it to the input desktop, which is the one
    // precondition a caller of this global-state write could otherwise get wrong.
    let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
        SetDisplayConfig(
            Some(paths.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY
                | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                | SDC_ALLOW_CHANGES
                | SDC_FORCE_MODE_ENUMERATION,
        )
    });
    if rc != 0 {
        tracing::debug!("force mode re-enumeration: SetDisplayConfig rc={rc:#x}");
    }
    rc == 0
}

/// The distinct resolutions `gdi_name` currently advertises (diagnostics for the in-place-resize
/// path: what the OS actually offers when a requested mode never shows up).
pub fn advertised_resolutions(gdi_name: &str) -> Vec<(u32, u32)> {
    let wname: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut set = std::collections::BTreeSet::new();
    let mut i = 0u32;
    loop {
        let mut dm = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: `wname` is a live NUL-terminated UTF-16 device name; `&mut dm` is a live,
        // size-stamped DEVMODEW the API fills for mode index `i`. Both outlive the call.
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
        set.insert((dm.dmPelsWidth, dm.dmPelsHeight));
        i += 1;
    }
    set.into_iter().collect()
}

/// Wait (bounded) until `gdi_name` ADVERTISES `mode`'s resolution in its display-mode list — the
/// gate between a driver-side mode-list refresh (`IOCTL_UPDATE_MODES`, latency plan P2) and the
/// CCD/GDI force-set: the OS re-evaluates an indirect display's settable modes asynchronously after
/// `IddCxMonitorUpdateModes2`, so an immediate `set_active_mode` could race the re-enumeration and
/// silently leave the old mode. Returns `true` once the requested `WxH@Hz` is enumerable.
///
/// The REFRESH is part of the match. It used to compare resolution only, which made a refresh-only
/// change (same WxH, new Hz) report "already advertised" — so the caller's fast path skipped the
/// `IOCTL_UPDATE_MODES` that would have taught the driver the new rate, `set_active_mode` fell back
/// to the best advertised rate <= requested, and the resize reported success at the OLD refresh.
pub fn wait_mode_advertised(gdi_name: &str, mode: Mode, ceiling: std::time::Duration) -> bool {
    let wname: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();
    let deadline = std::time::Instant::now() + ceiling;
    loop {
        let mut i = 0u32;
        loop {
            let mut dm = DEVMODEW {
                dmSize: size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            // SAFETY: `wname` is a live NUL-terminated UTF-16 device name whose pointer stays valid
            // for the call; `&mut dm` is a live, size-stamped DEVMODEW the API fills for mode index
            // `i`. Both outlive this synchronous call.
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
            if dm.dmPelsWidth == mode.width
                && dm.dmPelsHeight == mode.height
                && dm.dmDisplayFrequency == mode.refresh_hz
            {
                return true;
            }
            i += 1;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Monitor-departure wait (latency plan P0.3): after a REMOVE, poll until the target has left the
/// ACTIVE CCD set — two consecutive absent samples, so one transient query failure mid-teardown
/// can't read as "gone" — instead of sleeping the fixed departure settle. `ceiling` (the old fixed
/// sleep) bounds the worst case. The OS-side departure may still be finishing driver-side when the
/// CCD stops listing the target; the ADD path's ghost-reap retry (pf_vdisplay) remains the backstop
/// for that rare race, exactly as it was for a settle that expired. Returns `true` when departure
/// was observed, `false` on ceiling.
///
/// Call under the manager `state` lock like the callers it serves (serialization, not soundness).
pub fn wait_target_departed(target_id: u32, ceiling: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + ceiling;
    let mut absent_streak = 0u32;
    loop {
        if resolve_gdi_name(target_id).is_none() {
            absent_streak += 1;
            if absent_streak >= 2 {
                return true;
            }
        } else {
            absent_streak = 0;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Toggle the virtual-display target's advanced-color (HDR) state via the CCD API. Disabling HDR while on the
/// secure (Winlogon) desktop makes it render SDR/composed so DXGI Desktop Duplication can capture it
/// (the HDR fullscreen independent-flip otherwise storms `ACCESS_LOST` → black); re-enable on return so
/// WGC keeps HDR on the normal desktop. Returns true on a successful `DisplayConfigSetDeviceInfo`.
pub fn set_advanced_color(target_id: u32, enable: bool) -> bool {
    let mut np = 0u32;
    let mut nm = 0u32;
    // SAFETY: the CCD contract at the top of this file — `&mut np`/`&mut nm` are live
    // locals the OS fills with the counts it wants for these flags.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) }.is_err() {
        return false;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    // SAFETY: the CCD contract — `paths`/`modes` were just allocated with exactly `np`/`nm`
    // elements from the sizing call above, and are handed over with those same counts.
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        )
    }
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
                                               // SAFETY: `s.header` is a live local with `size` set to `DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE`'s
                                               // own `size_of` and `adapterId`/`id` copied from an active path the loop just matched; the
                                               // OS reads that many bytes and retains nothing.
            let rc = unsafe { DisplayConfigSetDeviceInfo(&s.header) };
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
pub fn advanced_color_enabled(target_id: u32) -> Option<bool> {
    let mut np = 0u32;
    let mut nm = 0u32;
    // SAFETY: the CCD contract at the top of this file — `&mut np`/`&mut nm` are live
    // locals the OS fills with the counts it wants for these flags.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) }.is_err() {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    // SAFETY: the CCD contract — `paths`/`modes` were just allocated with exactly `np`/`nm`
    // elements from the sizing call above, and are handed over with those same counts.
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        )
    }
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
            // SAFETY: `info.header` is a live local whose `size` field was just set to the
            // enclosing struct's own `size_of`, which is the contract that tells the OS how many bytes
            // it may touch; the struct outlives this synchronous call.
            if unsafe { DisplayConfigGetDeviceInfo(&mut info.header) } == 0 {
                // value bit 1 = advancedColorEnabled (bit 0 = advancedColorSupported).
                // SAFETY: POD union read (header) — `value` overlays a same-sized bitfield.
                return Some((unsafe { info.Anonymous.value } & 0x2) != 0);
            }
            return None;
        }
    }
    None
}

/// The target's SDR white level as a SCALE relative to 80 nits (`1.0` = 80 nits): where DWM
/// places SDR-white when composing SDR content onto this HDR desktop. An SDR-authored overlay
/// (the composited cursor) must be multiplied by this in scRGB space or it renders visibly
/// darker than the surrounding SDR desktop content (the Windows "SDR content brightness"
/// slider default alone is ~2.5x). `None` = query failed / target not active (callers keep
/// their last value or 1.0).
///
/// Read-only, over owned locals — same shape as [`advanced_color_enabled`].
pub fn sdr_white_level_scale(target_id: u32) -> Option<f32> {
    let mut np = 0u32;
    let mut nm = 0u32;
    // SAFETY: the CCD contract at the top of this file — `&mut np`/`&mut nm` are live
    // locals the OS fills with the counts it wants for these flags.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) }.is_err() {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    // SAFETY: the CCD contract — `paths`/`modes` were just allocated with exactly `np`/`nm`
    // elements from the sizing call above, and are handed over with those same counts.
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        )
    }
    .is_err()
    {
        return None;
    }
    for p in paths.iter().take(np as usize) {
        if p.targetInfo.id == target_id {
            let mut info = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
            info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
            info.header.size = size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
            info.header.adapterId = p.targetInfo.adapterId;
            info.header.id = p.targetInfo.id;
            // SAFETY: `info.header` is a live local whose `size` field was just set to the
            // enclosing struct's own `size_of`, which is the contract that tells the OS how many bytes
            // it may touch; the struct outlives this synchronous call.
            if unsafe { DisplayConfigGetDeviceInfo(&mut info.header) } == 0
                && info.SDRWhiteLevel > 0
            {
                // Contract: SDRWhiteLevel/1000 * 80 = nits, i.e. the /1000 IS the 80-nit scale.
                return Some(info.SDRWhiteLevel as f32 / 1000.0);
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
/// Force a REAL mode-set at the output's CURRENT mode — `CDS_RESET` applies even when nothing
/// changed (a plain re-apply of the same mode is treated as a no-op by the OS). This is the
/// presentation-restart hammer for a virtual display DWM silently stopped composing to after an
/// exclusive-eviction topology commit: measured on-glass, the eviction's forced re-commit
/// reassigned the swap-chain and the ring re-attach delivered the driver's stashed frame, but the
/// source's new-frame rate stayed 0 forever — only a real mode-set (the lever bring-up's ADD path
/// relies on via [`set_active_mode`]) makes the OS present again. Same input-desktop retry as the
/// mode-set proper. Returns `true` when the reset applied.
pub fn force_mode_reset(gdi_name: &str) -> bool {
    let wname: Vec<u16> = gdi_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `wname` is a live NUL-terminated UTF-16 device name and `&mut dm` a live DEVMODEW
    // out-param with `dmSize` set; the synchronous query only reads the name and fills `dm`.
    let ok =
        unsafe { EnumDisplaySettingsW(PCWSTR(wname.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }
            .as_bool();
    if !ok {
        tracing::warn!("{gdi_name}: force_mode_reset — no current mode to re-apply");
        return false;
    }
    // SAFETY: same liveness as the query above; CDS_RESET re-applies the identical mode, the two
    // trailing args are null, and the API only reads its inputs. The input-desktop retry mirrors
    // `set_active_mode` (a CDS write off the input desktop is refused with DISP_CHANGE_FAILED).
    let rc = crate::input_desktop::retry_on_input_desktop(
        |rc| *rc == DISP_CHANGE_FAILED,
        || unsafe {
            ChangeDisplaySettingsExW(PCWSTR(wname.as_ptr()), Some(&dm), None, CDS_RESET, None)
        },
    );
    if rc != DISP_CHANGE_SUCCESSFUL {
        tracing::warn!(
            result = rc.0,
            "{gdi_name}: force_mode_reset rejected ({})",
            disp_change_reason(rc.0)
        );
        return false;
    }
    tracing::info!("{gdi_name}: forced same-mode reset applied (presentation restart)");
    true
}

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
    // A CDS write from a thread that is not on the input desktop is refused with DISP_CHANGE_FAILED
    // (UAC consent / lock screen up) — retry it bound to that desktop rather than declaring the mode
    // unsupported, which is what stranded sessions on a black screen for a whole bring-up.
    let test = crate::input_desktop::retry_on_input_desktop(
        |rc| *rc == DISP_CHANGE_FAILED,
        || unsafe {
            ChangeDisplaySettingsExW(PCWSTR(wname.as_ptr()), Some(&dm), None, CDS_TEST, None)
        },
    );
    if test != DISP_CHANGE_SUCCESSFUL {
        tracing::warn!(
            result = test.0,
            "{gdi_name}: mode-set {}x{}@{} rejected ({}) — leaving OS default",
            mode.width,
            mode.height,
            chosen_hz,
            disp_change_reason(test.0)
        );
        return;
    }
    // SAFETY: same inputs as the CDS_TEST call above — `wname` (live NUL-terminated device name) and
    // `&dm` (live DEVMODEW) both outlive the call; CDS_UPDATEREGISTRY applies the already-validated mode,
    // and the API only reads its inputs.
    // Same wrong-desktop retry as the validate above: the two calls bind independently, so an apply
    // still lands even when the secure desktop came up between them.
    let apply = crate::input_desktop::retry_on_input_desktop(
        |rc| *rc == DISP_CHANGE_FAILED,
        || unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(wname.as_ptr()),
                Some(&dm),
                None,
                CDS_UPDATEREGISTRY,
                None,
            )
        },
    );
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
            "{gdi_name}: failed to apply {}x{}@{} ({})",
            mode.width,
            mode.height,
            chosen_hz,
            disp_change_reason(apply.0)
        );
    }
}

/// Human decode for a failed `ChangeDisplaySettingsExW` result. The two codes worth telling apart
/// in a field log: `BADMODE` (the display's mode list genuinely lacks the mode) vs `FAILED` (the
/// write itself was rejected). An earlier revision printed "mode not advertised?" for BOTH, which
/// sent a lid-closed field report chasing the wrong cause.
///
/// `FAILED` itself has two causes needing opposite fixes — no console-session access (disconnected
/// RDP) versus the right session but the wrong desktop (UAC / lock / logon screen owns input) — so
/// it asks which before naming one. See [`sdc_access_denied_hint`] for the same split on the CCD
/// side.
fn disp_change_reason(rc: i32) -> &'static str {
    match rc {
        -1 if crate::input_desktop::input_desktop_is_secure() => {
            "DISP_CHANGE_FAILED: the SECURE desktop owns input — a UAC consent prompt, the lock \
             screen or the logon screen is up, and display writes are refused off it. Dismiss the \
             prompt on the host"
        }
        -1 => {
            "DISP_CHANGE_FAILED: the display write was rejected — a host without console-session \
             access (disconnected RDP session / non-console session) fails ALL display writes \
             this way"
        }
        -2 => "DISP_CHANGE_BADMODE: the display does not advertise this mode",
        -3 => "DISP_CHANGE_NOTUPDATED: registry write failed",
        -4 => "DISP_CHANGE_BADFLAGS",
        -5 => "DISP_CHANGE_BADPARAM",
        _ => "unrecognized DISP_CHANGE code",
    }
}

/// Appended to `SetDisplayConfig` failure logs when rc is `ERROR_ACCESS_DENIED` (0x5). Every other
/// rc gets no hint.
///
/// `ERROR_ACCESS_DENIED` has TWO field causes and they need opposite fixes, so ask which one before
/// naming it. MS docs say only "the caller does not have access to the console session", which is
/// the disconnected-RDP / non-console case — but the SAME rc comes back when the host IS in the
/// console session and merely off the input desktop (UAC consent, lock or logon screen up). Naming
/// only the first sent a 2026-07-23 investigation chasing a phantom RDP session while a consent
/// prompt was the actual cause, on a host the message told to "run via the installed service" —
/// which it already was. [`crate::input_desktop`] now retries these bound to the input desktop, so
/// seeing this at all means even that did not help.
fn sdc_access_denied_hint(rc: i32) -> &'static str {
    if rc != 5 {
        return "";
    }
    if crate::input_desktop::input_desktop_is_secure() {
        " (ERROR_ACCESS_DENIED: the SECURE desktop owns input — a UAC consent prompt, the lock \
         screen or the logon screen is up, and display writes are refused off it. Dismiss the \
         prompt on the host)"
    } else {
        " (ERROR_ACCESS_DENIED: the host has no console-session access — disconnected RDP \
         session? run via the installed service so it tracks the console session)"
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
fn query_active_config() -> Option<SavedConfig> {
    let mut np = 0u32;
    let mut nm = 0u32;
    // SAFETY: the CCD contract at the top of this file — `&mut np`/`&mut nm` are live
    // locals the OS fills with the counts it wants for these flags.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) }.is_err() {
        return None;
    }
    // Zero active paths is an ANSWER, not a failure — and an ordinary state: every panel off or in
    // standby, a KVM switched away, a headless box between the adapter arriving and its first
    // monitor. `QueryDisplayConfig` REJECTS a zero-count call rather than returning an empty set,
    // so asking anyway turns "nothing is active" into "the query failed". Measured on .173
    // (RTX 4090, Win11 26200, TV powered off — every monitor devnode Code 45): the sizing call
    // succeeds with numPaths=0 and the query then returns 0x57 ERROR_INVALID_PARAMETER in a live
    // logged-on console session, 0x5 ERROR_ACCESS_DENIED from session 0. That `None` is what makes
    // `isolate_displays_ccd` report failure, which is the condition whose recovery legs exist to
    // stop the operator's panels being left dark.
    if np == 0 {
        return Some((Vec::new(), Vec::new()));
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    // SAFETY: the CCD contract — `paths`/`modes` were just allocated with exactly `np`/`nm`
    // elements from the sizing call above, and are handed over with those same counts.
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        )
    }
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
pub fn count_other_active(keep_target_ids: &[u32]) -> Option<u32> {
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
/// attribution inventory. `external_physical` and `internal_panel` are the load-bearing bits: a
/// standby TV/monitor on a real connector is the classic suspect for the periodic link-probe
/// stutter class, and a laptop panel the exclusive isolate DEACTIVATED is the hybrid-laptop
/// variant of the same disturbance (field A/B 2026-07-27: dark-but-connected eDP head on the
/// iGPU → ~2 s stall metronome; `topology: primary` → zero) — only indirect/virtual targets
/// (our own IDD included) can never be suspects.
pub struct TargetInventory {
    pub target_id: u32,
    /// Whether any active path drives this target (part of the desktop right now).
    pub active: bool,
    /// External physical connector (HDMI/DP/DVI/…): candidate for standby link-probe churn.
    pub external_physical: bool,
    /// Internal panel (eDP/LVDS/embedded): candidate for dark-head servicing churn when
    /// connected-but-inactive (the exclusive isolate on a laptop creates exactly that state).
    pub internal_panel: bool,
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
pub fn target_inventory() -> Vec<TargetInventory> {
    let mut np = 0u32;
    let mut nm = 0u32;
    // SAFETY: the CCD contract at the top of this file — `&mut np`/`&mut nm` are live
    // locals the OS fills with the counts it wants for these flags.
    if unsafe { GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut np, &mut nm) }.is_err() {
        return Vec::new();
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    // SAFETY: the CCD contract — `paths`/`modes` were just allocated with exactly `np`/`nm`
    // elements from the sizing call above, and are handed over with those same counts.
    if unsafe {
        QueryDisplayConfig(
            QDC_ALL_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        )
    }
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
        // SAFETY: `req.header` is a live local whose `size` field was just set to the
        // enclosing struct's own `size_of`, which is the contract that tells the OS how many bytes
        // it may touch; the struct outlives this synchronous call.
        if unsafe { DisplayConfigGetDeviceInfo(&mut req.header) } != 0 {
            continue; // target with no queryable monitor — nothing to attribute to
        }
        let (external_physical, tech) = output_tech_class(req.outputTechnology);
        out.push(TargetInventory {
            target_id: t.id,
            active: active.contains(&key),
            external_physical,
            internal_panel: tech == "internal-panel",
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
pub fn isolate_displays_ccd(keep_target_ids: &[u32]) -> Option<SavedConfig> {
    // Snapshot the ORIGINAL active config ONCE for restore-on-teardown, before any changes.
    let saved = query_active_config()?;

    // Nothing is active, so there is nothing to deactivate and nothing to restore later. Say so by
    // returning the empty snapshot rather than falling into the retry loop: the re-commit below is
    // there to drive the IddCx adapter's COMMIT_MODES, and with no path at all there is no config
    // to commit — a zero-path `SetDisplayConfig` is simply rejected, and the four attempts would
    // log an apply failure and a 250 ms sleep each for a topology that is already what we want.
    // `restore_displays_ccd` already treats an empty config as the no-op it is.
    if saved.0.is_empty() {
        tracing::info!(
            "display isolate (CCD): no display path is active — nothing to isolate for target set \
             {keep_target_ids:?} (every panel off/standby, or a headless host)"
        );
        return Some(saved);
    }

    // Deactivate every non-keep display, then VERIFY and RETRY. A field-reported bug had a physical
    // monitor STAY ACTIVE in exclusive mode, so we don't trust a single SetDisplayConfig: re-query the
    // live topology each attempt and re-apply until ONLY the keep set is active. Secure-desktop
    // correctness depends on this — the lock screen must not land on a stray panel while we stream.
    for attempt in 1..=4u32 {
        let (mut paths, mut modes) = query_active_config()?;
        let mut others = 0u32;
        for p in paths.iter_mut() {
            if keep_target_ids.contains(&p.targetInfo.id) {
                continue;
            }
            if p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 {
                // Mark the path inactive AND unpin its modes: per the SetDisplayConfig
                // contract a path being turned OFF needs BOTH mode indexes marked invalid,
                // and leaving them referencing the queried mode entries gets the whole
                // supplied config rejected with 0x57 ERROR_INVALID_PARAMETER on some
                // driver/topology combinations (field-reported: exclusive mode left the
                // physical panel lit, every retry failing 0x57). Writing the all-ones
                // sentinel to the whole union is also correct under the virtual-mode-aware
                // interpretation (cloneGroupId/sourceModeInfoIdx both become their 0xffff
                // INVALID values).
                p.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
                p.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
                p.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
                others += 1;
            }
        }
        // The doomed display may have HELD the desktop origin (the physical is primary in the
        // single-slot exclusive case): re-anchor the kept sources so the supplied config still
        // contains a primary — an origin-less desktop is rejected 0x57 no matter which array
        // shape carries it (see `anchor_kept_sources_at_origin`).
        if others > 0 {
            anchor_kept_sources_at_origin(&paths, &mut modes);
        }
        // Commit the config. Even when nothing needed deactivating we re-commit: a legacy mode-set does
        // NOT drive the IddCx adapter's EVT_IDD_CX_ADAPTER_COMMIT_MODES, and without COMMIT_MODES the OS
        // never calls ASSIGN_SWAPCHAIN, so the driver receives no frames. SDC_FORCE_MODE_ENUMERATION
        // forces the re-commit; SAVE_TO_DATABASE only in the sole-path case (matches prior behavior —
        // don't permanently rewrite the user's multi-display layout; the teardown restore handles it).
        // The supplied shape is decided (and its escalation announced) ONCE, outside the write, so a
        // wrong-desktop retry re-issues the identical config instead of logging the escalation twice.
        let keep_only = (others > 0 && attempt >= 2).then(|| {
            // ESCALATION (attempt 2+): supply ONLY the keep paths. Kept as belt-and-braces —
            // the field 0x57 this was built for turned out to be the missing desktop origin
            // (see `anchor_kept_sources_at_origin`), which rejected BOTH shapes identically;
            // but should some other validator still choke on the full array, the minimal
            // shape is the best last word. The final attempt also drops
            // SDC_FORCE_MODE_ENUMERATION in case the driver rejects it combined with a real
            // topology change — an actual path removal drives COMMIT_MODES on its own, so the
            // re-commit rationale doesn't need the flag here.
            let (kp, km) = keep_only_supplied(&paths, &modes);
            let mut esc = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
            if attempt < 4 {
                esc |= SDC_FORCE_MODE_ENUMERATION;
            }
            tracing::info!(
                "display isolate (CCD): escalating to a keep-only supplied config (attempt {attempt}/4, paths {}→{}, modes {}→{})",
                paths.len(), kp.len(), modes.len(), km.len()
            );
            (kp, km, esc)
        });
        // SAFETY: the CCD contract — both arms hand over slices (so pointer and length agree)
        // that outlive the call, and `retry_set_display_config` binds the write to the input
        // desktop. `keep_only`'s arrays are a `SavedConfig` this module built from a prior
        // `QueryDisplayConfig`, never caller-supplied.
        let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
            match &keep_only {
                Some((kp, km, esc)) => {
                    SetDisplayConfig(Some(kp.as_slice()), Some(km.as_slice()), *esc)
                }
                None => {
                    let mut flags = SDC_APPLY
                        | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                        | SDC_ALLOW_CHANGES
                        | SDC_FORCE_MODE_ENUMERATION;
                    if others == 0 {
                        flags |= SDC_SAVE_TO_DATABASE;
                    }
                    SetDisplayConfig(Some(paths.as_slice()), Some(modes.as_slice()), flags)
                }
            }
        });
        // A failed apply must be VISIBLE even when the verification below passes vacuously (nothing
        // else was active to deactivate — the lid-closed laptop case, where the success INFO used to
        // swallow rc=0x5): the re-commit above is load-bearing (COMMIT_MODES → ASSIGN_SWAPCHAIN),
        // and a denied apply means it never drove the IddCx adapter.
        if rc != 0 {
            tracing::warn!(
                "display isolate (CCD): SetDisplayConfig rc={rc:#x}{} — the re-commit did NOT \
                 apply (COMMIT_MODES/ASSIGN_SWAPCHAIN may not fire for the virtual display)",
                sdc_access_denied_hint(rc)
            );
        }

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
    // Name the survivors instead of assuming their kind — the field logs showed this path fire
    // with a sibling VIRTUAL display as the survivor (linger-expiry shrink), where the old
    // "a non-virtual display stayed active" wording sent the triage the wrong way.
    let survivors: Vec<String> = target_inventory()
        .iter()
        .filter(|t| t.active && !keep_target_ids.contains(&t.target_id))
        .map(|t| format!("{} {} \"{}\"", t.target_id, t.tech, t.friendly))
        .collect();
    tracing::error!(
        "display isolate (CCD): failed to isolate target set {keep_target_ids:?} after 4 attempts — still active: [{}] (field-reported exclusive-mode bug)",
        survivors.join(", ")
    );
    Some(saved)
}

// (A "gentle" eviction variant — deactivate the stray paths WITHOUT `SDC_FORCE_MODE_ENUMERATION`,
// kept paths supplied verbatim — was tried for the re-assert watchdog and REMOVED: on-glass it
// still bounced the live virtual display's swap-chain (a real topology change drives COMMIT_MODES
// on its own) AND additionally left the OS not presenting to the virtual display — capture
// received one stashed frame and then nothing. Eviction therefore always goes through
// [`isolate_displays_ccd`], whose forced re-commit restarts presentation, and the SESSION pairs it
// with an in-place capture re-attach; see the vdisplay manager's re-assert watchdog.)

/// Build the ESCALATED supplied config for [`isolate_displays_ccd`]: ONLY the paths still flagged
/// ACTIVE (the keep set — the caller already cleared ACTIVE on every doomed path), with the mode
/// table rebuilt to just the entries those paths reference (indexes remapped). Docs-wise the
/// dropped inactive entries were declared ignored anyway ("Only the paths within this array that
/// have the DISPLAYCONFIG_PATH_ACTIVE flag set are set"), so this shape asks for the identical
/// topology — minus the array contents some driver/OS validation combos reject with 0x57.
fn keep_only_supplied(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> (Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>) {
    let mut out_paths = Vec::new();
    let mut out_modes = Vec::new();
    // old mode index → new. Shared entries dedup through here: a clone-style pair references ONE
    // source mode, and the docs require each source/target mode to appear in the table only once.
    let mut remap = std::collections::HashMap::new();
    for p in paths {
        if p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        let mut q = *p;
        q.sourceInfo.Anonymous.modeInfoIdx = remap_mode_idx(
            // SAFETY: POD union read (header) — diagnostics only.
            unsafe { q.sourceInfo.Anonymous.modeInfoIdx },
            modes,
            &mut out_modes,
            &mut remap,
        );
        q.targetInfo.Anonymous.modeInfoIdx = remap_mode_idx(
            // SAFETY: POD union read (header) — diagnostics only.
            unsafe { q.targetInfo.Anonymous.modeInfoIdx },
            modes,
            &mut out_modes,
            &mut remap,
        );
        out_paths.push(q);
    }
    (out_paths, out_modes)
}

/// Move `modes[old]` into `out` (once — `remap` dedups) and return its new index. INVALID and
/// out-of-range indexes stay INVALID — `SDC_ALLOW_CHANGES` lets best-mode logic fill the gap.
fn remap_mode_idx(
    old: u32,
    modes: &[DISPLAYCONFIG_MODE_INFO],
    out: &mut Vec<DISPLAYCONFIG_MODE_INFO>,
    remap: &mut std::collections::HashMap<u32, u32>,
) -> u32 {
    if old == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
        return old;
    }
    let Some(m) = modes.get(old as usize) else {
        return DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    };
    *remap.entry(old).or_insert_with(|| {
        out.push(*m);
        (out.len() - 1) as u32
    })
}

/// A committable desktop must still contain a PRIMARY — a source pinned exactly at the origin
/// `(0,0)`. Deactivating the display that held the origin (the physical, in the exclusive
/// topology) while the kept virtual stays pinned at its EXTEND offset supplies an origin-less
/// desktop, and Windows rejects that wholesale with 0x57 ERROR_INVALID_PARAMETER no matter the
/// array shape — the field box failed identically with the doomed path carried inactive AND with
/// the keep-only escalation, yet the very same call converged rc=0 whenever a kept member already
/// sat at `(0,0)`; the origin was the real variable all along. Translate the kept sources RIGIDLY
/// (relative arrangement preserved) so the top-left-most lands exactly on the origin. A set that
/// already covers `(0,0)` is left untouched, so a plain re-commit stays byte-identical and a
/// user's negative-coordinate multi-monitor layout is never rearranged.
fn anchor_kept_sources_at_origin(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &mut [DISPLAYCONFIG_MODE_INFO],
) {
    // Unique source-mode entries of the still-ACTIVE (kept) paths — clone-style pairs share one,
    // and a shared entry must be translated once.
    let mut idxs: Vec<usize> = Vec::new();
    for p in paths {
        if p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        // SAFETY: POD union read (header) — `modeInfoIdx` overlays a same-sized bitfield struct,
        // both valid for every bit pattern. The index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        let Some(m) = modes.get(idx) else { continue };
        if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE && !idxs.contains(&idx) {
            idxs.push(idx);
        }
    }
    let positions: Vec<(i32, i32)> = idxs
        .iter()
        .map(|&i| {
            // SAFETY: discriminated union read (header) — `idxs` was filtered on
            // `infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE` when it was built above.
            let pos = unsafe { modes[i].Anonymous.sourceMode.position };
            (pos.x, pos.y)
        })
        .collect();
    if positions.contains(&(0, 0)) {
        return; // the kept set already holds the primary — don't touch a valid layout
    }
    // Lexicographic min over the actual positions — the anchor IS one of the kept sources, so
    // after translation one source sits exactly at (0,0), which is what the validator wants.
    let Some((ax, ay)) = positions.iter().copied().min() else {
        return; // no pinned kept sources — placement is the OS's (SDC_ALLOW_CHANGES), nothing to anchor
    };
    for &i in &idxs {
        // SAFETY: discriminated union write (header) — same `idxs`, same `infoType` filter.
        let sm = unsafe { &mut modes[i].Anonymous.sourceMode };
        sm.position.x -= ax;
        sm.position.y -= ay;
    }
    tracing::info!(
        "display isolate (CCD): kept source(s) re-anchored onto the desktop origin (primary) — the doomed display held (0,0) delta=({},{})",
        -ax,
        -ay
    );
}

/// The desktop-space rectangle `(x, y, w, h)` of `target_id`'s SOURCE — where this display's
/// region lives in the desktop coordinate space. `None` while the target isn't an active path.
/// Used by the IDD-push compose kick to dirty THE TARGET display: with parallel displays the
/// cursor sits on ONE of them, and a cursor wiggle only dirties that one — a sibling display's
/// kick must first know where to send the cursor (Stage W3 on-glass finding).
pub fn source_desktop_rect(target_id: u32) -> Option<(i32, i32, i32, i32)> {
    // SAFETY: `query_active_config` is this module's own CCD helper: it takes nothing and returns owned
    // `Vec`s built from a fresh `QueryDisplayConfig`, so it has no caller obligation at all.
    let (paths, modes) = query_active_config()?;
    for p in &paths {
        if p.targetInfo.id != target_id || p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        // SAFETY: POD union read (header) — `modeInfoIdx` overlays a same-sized bitfield struct,
        // both valid for every bit pattern. The index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        let m = modes.get(idx)?;
        if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            return None;
        }
        // SAFETY: discriminated union read (header) — guarded by `infoType ==
        // DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE` on this same entry.
        let sm = unsafe { m.Anonymous.sourceMode };
        return Some((
            sm.position.x,
            sm.position.y,
            sm.width as i32,
            sm.height as i32,
        ));
    }
    None
}

/// The union of every ACTIVE path's source rect — the virtual-desktop bounds `(x, y, w, h)` in
/// desktop coordinates. Read from CCD rather than `GetSystemMetrics(SM_*VIRTUALSCREEN)` so the
/// answer is the CONSOLE's real layout even when the calling process sits in another session (the
/// GDI metrics are a per-session view; the CCD database is global). `None` when nothing is active
/// or the query fails. Used to normalize desktop coordinates into the virtual HID pointer's
/// absolute `0..=32767` axis space (win32k maps the device's logical extents onto the virtual
/// screen).
pub fn desktop_bounds() -> Option<(i32, i32, i32, i32)> {
    let (paths, modes) = query_active_config()?;
    let mut acc: Option<(i32, i32, i32, i32)> = None; // (x0, y0, x1, y1)
    for p in &paths {
        if p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        // SAFETY: POD union read (header) — `modeInfoIdx` overlays a same-sized bitfield struct,
        // both valid for every bit pattern. The index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        let Some(m) = modes.get(idx) else { continue };
        if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            continue;
        }
        // SAFETY: discriminated union read (header) — guarded by `infoType ==
        // DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE` on this same entry.
        let sm = unsafe { m.Anonymous.sourceMode };
        let (x0, y0) = (sm.position.x, sm.position.y);
        let (x1, y1) = (x0 + sm.width as i32, y0 + sm.height as i32);
        acc = Some(match acc {
            None => (x0, y0, x1, y1),
            Some((ax0, ay0, ax1, ay1)) => (ax0.min(x0), ay0.min(y0), ax1.max(x1), ay1.max(y1)),
        });
    }
    acc.map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0, y1 - y0))
}

/// Place each managed virtual target's SOURCE at the given desktop-space origin, as ONE atomic CCD
/// `SetDisplayConfig` (design `display-management.md` §6.2 — the Windows arm of the pure
/// `vdisplay/layout.rs` arrangement; positions come from `arrange`, this only commits them). Windows
/// treats the source at `(0,0)` as primary, so auto-row's first member lands primary — the group's
/// designated member. Paths not named stay where they are. Best-effort: a failure leaves the OS
/// placement (mouse crossing may not match the layout table until the next apply).
pub fn apply_source_positions(positions: &[(u32, i32, i32)]) {
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
        // SAFETY: POD union read (header) — `modeInfoIdx` overlays a same-sized bitfield struct,
        // both valid for every bit pattern. The index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
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
    // SAFETY: the CCD contract at the top of this file — the path/mode arrays go over as
    // slices, so pointer and length cannot disagree, and both outlive this synchronous
    // call. `retry_set_display_config` binds it to the input desktop, which is the one
    // precondition a caller of this global-state write could otherwise get wrong.
    let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
        SetDisplayConfig(
            Some(paths.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY
                | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                | SDC_ALLOW_CHANGES
                | SDC_FORCE_MODE_ENUMERATION,
        )
    });
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
pub fn set_virtual_primary_ccd(keep_target_id: u32) -> Option<SavedConfig> {
    // Through the shared query, not a private copy of it. This was a verbatim duplicate of
    // `query_active_config` — same flags, same shape — and so the one CCD entry point that did not
    // inherit its zero-path fix (the seam asymmetry this crate keeps producing: N-1 of N sibling
    // paths share a helper and the Nth open-codes it).
    let (paths, mut modes) = query_active_config()?;
    let saved = (paths.clone(), modes.clone());

    // The virtual output's source width, to lay the other displays out to its right.
    let virt_width = paths.iter().find_map(|p| {
        if p.targetInfo.id != keep_target_id {
            return None;
        }
        // SAFETY: POD union read (header) — `modeInfoIdx` overlays a same-sized bitfield struct,
        // both valid for every bit pattern. The index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
        let m = modes.get(idx)?;
        // `then_some` (eager): `sourceMode.width` is a POD `u32` union read, discarded when the arm is
        // false — no lazy guard needed. (`then(|| …)` here trips clippy::unnecessary_lazy_evaluations.)
        // SAFETY: discriminated union read (header). The `infoType` test is the same expression's
        // receiver, so it has already been evaluated — but note it does NOT gate the read, which is
        // why the POD-ness above is what actually justifies this one.
        let width = unsafe { m.Anonymous.sourceMode.width } as i32;
        (m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE).then_some(width)
    })?;
    let others = paths.len().saturating_sub(1);

    // Reposition each active path's SOURCE once: the virtual to (0,0) (= primary), the other
    // displays PACKED left-to-right from the virtual's right edge — kept active, no overlap and no
    // gap (vs. blindly shifting each by virt_width, which leaves a dead gap when EXTEND already
    // placed them to the right). Dedup source-mode indices (a cloned group shares one).
    let mut next_x = virt_width;
    let mut done = std::collections::HashSet::new();
    for p in paths.iter() {
        // SAFETY: POD union read (header) — `modeInfoIdx` overlays a same-sized bitfield struct,
        // both valid for every bit pattern. The index is bounds-checked below.
        let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
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
            // (A union field ASSIGNMENT needs no `unsafe` — only reads do.)
            m.Anonymous.sourceMode.position = POINTL { x: 0, y: 0 };
        } else {
            // SAFETY: discriminated union READ (header) — `infoType` checked immediately above.
            // (The assignment on the next line needs no `unsafe`; only reads do.)
            let w = unsafe { m.Anonymous.sourceMode.width } as i32;
            m.Anonymous.sourceMode.position = POINTL { x: next_x, y: 0 };
            next_x += w;
        }
    }

    // SAFETY: the CCD contract at the top of this file — the path/mode arrays go over as
    // slices, so pointer and length cannot disagree, and both outlive this synchronous
    // call. `retry_set_display_config` binds it to the input desktop, which is the one
    // precondition a caller of this global-state write could otherwise get wrong.
    let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
        SetDisplayConfig(
            Some(paths.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY
                | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                | SDC_ALLOW_CHANGES
                | SDC_FORCE_MODE_ENUMERATION,
        )
    });
    if rc == 0 {
        tracing::info!("display primary (CCD): virtual target {keep_target_id} set PRIMARY at (0,0); {others} other display(s) kept ACTIVE + packed to its right");
    } else {
        tracing::warn!(
            "display primary (CCD): SetDisplayConfig failed rc={rc:#x}{} (virtual {keep_target_id} primary, physicals kept)",
            sdc_access_denied_hint(rc)
        );
    }
    Some(saved)
}

/// Restore the topology saved by [`isolate_displays_ccd`] (teardown, before the virtual output is
/// removed), re-activating the displays we deactivated.
// pub so vdisplay::pf_vdisplay can reuse this backend-neutral CCD restore helper.
pub fn restore_displays_ccd(saved: &SavedConfig) {
    let (paths, modes) = saved;
    if paths.is_empty() {
        return;
    }
    // SAFETY: the CCD contract at the top of this file — the path/mode arrays go over as
    // slices, so pointer and length cannot disagree, and both outlive this synchronous
    // call. `retry_set_display_config` binds it to the input desktop, which is the one
    // precondition a caller of this global-state write could otherwise get wrong.
    let rc = crate::input_desktop::retry_set_display_config(|| unsafe {
        SetDisplayConfig(
            Some(paths.as_slice()),
            Some(modes.as_slice()),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        )
    });
    if rc == 0 {
        tracing::info!("display isolate (CCD): restored original topology");
    } else {
        tracing::warn!(
            "display isolate (CCD): topology restore failed rc={rc:#x}{} — physical displays may be left deactivated",
            sdc_access_denied_hint(rc)
        );
    }
    // GUARANTEE the desk is never left all-dark. The saved config can be unappliable (field
    // rc=0x64a ERROR_BAD_CONFIGURATION: it pinned a virtual target incarnation that was since
    // removed) or even apply rc=0 yet re-light nothing (snapshotted while an earlier failed
    // teardown had the physicals off — the poisoned-snapshot chain from the field logs). Either
    // way, if no external physical panel is active after the apply while at least one is
    // connected, fall back to the OS database EXTEND preset, which re-activates every connected
    // display. Internal panels deliberately don't count as lit-able here — a closed clamshell
    // lid must not be forced back on.
    let (connected, lit) = target_inventory()
        .iter()
        .filter(|t| t.external_physical)
        .fold((0u32, 0u32), |(c, a), t| (c + 1, a + u32::from(t.active)));
    if connected > 0 && lit == 0 {
        tracing::warn!(
            "display isolate (CCD): no external physical display active after the restore (rc={rc:#x}, connected={connected}) — forcing the EXTEND preset so the desk is not left dark"
        );
        force_extend_topology();
    }
}

/// This file's first tests. Everything here reads the LIVE display topology, so each case is
/// `#[ignore]`d and reports `ignored` rather than a vacuous `ok` when nobody runs it on hardware —
/// the shape Phase 0.3 of the pf-vdisplay sweep settled on after finding env-guarded early-returns
/// reporting success without executing.
///
/// Run with `cargo test -p pf-win-display -- --ignored` on a real box.
///
/// ⚠️ Read-only by construction, and it must stay that way. The obvious companion assertion —
/// `isolate_displays_ccd(&[])` — is NOT written here on purpose: an empty keep set means "keep
/// nothing", so on a box with displays it would deactivate every one of them and blank the
/// operator's desk. The query below is the safe half of the same evidence.
#[cfg(test)]
mod live_tests {
    use super::*;

    /// A CCD query must never report FAILURE for a host that simply has nothing lit.
    ///
    /// `GetDisplayConfigBufferSizes` answers `numPaths = 0` for an ordinary state — every panel off
    /// or in standby, a KVM switched away, a headless box. `QueryDisplayConfig` then rejects the
    /// zero-count call rather than handing back an empty set, so asking anyway converted "nothing
    /// is active" into "the query failed". That `None` propagates: `isolate_displays_ccd` returns
    /// `None`, which is the teardown-gate condition whose recovery legs exist precisely to stop the
    /// operator's panels being left dark.
    ///
    /// Verified on .173 (RTX 4090, Win11 26200) with the TV powered off — every monitor devnode
    /// Code 45, `numPaths = 0` for `QDC_ALL_PATHS` as well — in a live logged-on console session,
    /// where the query returned 0x57 ERROR_INVALID_PARAMETER (and 0x5 ERROR_ACCESS_DENIED from
    /// session 0). This case FAILS there without the zero-path short-circuit and passes with it,
    /// while a box that does have a display lit passes either way.
    #[test]
    #[ignore = "hardware: reads the live display topology"]
    fn a_host_with_nothing_lit_reports_zero_actives_rather_than_a_failed_query() {
        let n = count_other_active(&[]).expect(
            "count_other_active returned None, i.e. the CCD query was reported as FAILED. On a \
             host with no active display path that is the zero-path conflation, and it is what \
             makes isolate_displays_ccd yield None on a perfectly healthy machine",
        );
        tracing::info!("live CCD query: {n} active display path(s)");
    }
}
