//! PnP monitor-devnode disable — the EXPERIMENTAL `pnp_disable_monitors` display-policy axis.
//!
//! An `Exclusive` isolate removes the physical monitors from the desktop TOPOLOGY (CCD), but their
//! PnP device nodes stay live — so a standby monitor/TV that periodically wakes its connection
//! (auto input scan, Instant-On HPD cycling, DP link events) still triggers the full Windows
//! reaction each time: PnP arrival/removal, CCD re-evaluation, DWM invalidation. Field evidence
//! (Apollo #368's Device-Manager refresh at every hitch; our own reporter's TV where unplugging the
//! cable removes a metronomic ~4 s double-jolt) says this reaction cascade is the expensive part.
//!
//! This module disables physical monitors' devnodes for the stream's duration
//! (`CM_Disable_DevNode` with `CM_DISABLE_PERSIST`, so a devnode that hot-plug re-arrives STAYS
//! disabled — that persistence is the whole point) and re-enables them at teardown before the CCD
//! restore. Two precise selectors, never "every monitor but ours" (co-installed third-party
//! virtual displays are untouched): [`disable_for_deactivated`] — monitors on targets the
//! `Exclusive` isolate actually deactivated, mapped CCD target → monitor device interface path
//! (`DISPLAYCONFIG_TARGET_DEVICE_NAME`) → PnP instance id; and [`disable_connected_inactive`] —
//! external physical monitors that are connected but not part of the desktop in ANY topology (the
//! standby-TV case: never active, so the first selector can't see it, yet it probes the link all
//! the same — the exact class in the field reports above).
//!
//! Crash safety: instance ids are journaled to `<config>/pnp-disabled-monitors.json` BEFORE the
//! disable and cleared after a successful re-enable; [`startup_recover`] re-enables leftovers when
//! the host starts after a crash/kill. Worst case (host dies AND never restarts) the monitor stays
//! disabled until the user re-enables it in Device Manager — the web-console help text says so.

use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Disable_DevNode, CM_Enable_DevNode, CM_Locate_DevNodeW, CM_DISABLE_PERSIST,
    CM_LOCATE_DEVNODE_NORMAL, CM_LOCATE_DEVNODE_PHANTOM, CR_SUCCESS,
};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME,
};
use windows::Win32::Foundation::LUID;

/// The crash-recovery journal: PnP instance ids we disabled and have not yet re-enabled.
fn journal_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("pnp-disabled-monitors.json")
}

fn read_journal() -> Vec<String> {
    match std::fs::read(journal_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Persist `ids` as the outstanding-disable set (union semantics handled by the callers). Failure
/// is logged, not fatal — the feature degrades to "no crash journal", not "no feature".
fn write_journal(ids: &[String]) {
    let path = journal_path();
    if ids.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = pf_paths::create_private_dir(dir);
    }
    if let Err(e) = std::fs::write(&path, serde_json::to_vec_pretty(&ids).unwrap_or_default()) {
        tracing::warn!(error = %e, "PnP-disable: could not write the crash-recovery journal");
    }
}

/// `\\?\DISPLAY#GSM83CD#5&367fb4cb&0&UID4352#{guid}` → `DISPLAY\GSM83CD\5&367fb4cb&0&UID4352`.
/// The standard device-interface-path → instance-id transform: strip the `\\?\` prefix and the
/// trailing `#{interface-class-guid}`, then `#` separators become `\`.
// pub(crate): `display_events` applies the same transform to DBT_DEVICEARRIVAL interface paths.
pub fn instance_id_from_interface_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\")?;
    let cut = rest.rfind("#{")?;
    Some(rest[..cut].replace('#', "\\"))
}

fn utf16z(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Resolve a CCD target to its monitor's (instance id, friendly name).
fn monitor_instance(adapter: LUID, target_id: u32) -> Option<(String, String)> {
    let mut req = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
    req.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
    req.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
    req.header.adapterId = adapter;
    req.header.id = target_id;
    // SAFETY: `req` is a properly-sized DISPLAYCONFIG_TARGET_DEVICE_NAME local whose header
    // (type/size/adapterId/id) is fully initialised; the API writes only within the struct.
    let rc = unsafe { DisplayConfigGetDeviceInfo(&mut req.header) };
    if rc != 0 {
        return None;
    }
    let id = instance_id_from_interface_path(&utf16z(&req.monitorDevicePath))?;
    Some((id, utf16z(&req.monitorFriendlyDeviceName)))
}

/// Apply enable/disable to one PnP instance id. Returns whether the config action succeeded.
fn set_devnode(id: &str, disable: bool) -> bool {
    let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
    let mut devinst = 0u32;
    // A disabled (or currently-departed) devnode may not be in the live tree — locate PHANTOM for
    // the enable path so recovery still finds it; the disable path requires a present device.
    let flags = if disable {
        CM_LOCATE_DEVNODE_NORMAL
    } else {
        CM_LOCATE_DEVNODE_PHANTOM
    };
    // SAFETY: `wide` is a live NUL-terminated UTF-16 instance id outliving the call; `devinst` is
    // a valid out-param.
    let cr = unsafe { CM_Locate_DevNodeW(&mut devinst, PCWSTR(wide.as_ptr()), flags) };
    if cr != CR_SUCCESS {
        tracing::warn!(id, cr = cr.0, "PnP-disable: CM_Locate_DevNodeW failed");
        return false;
    }
    // SAFETY: `devinst` is the devnode the locate above resolved; plain value flags.
    let cr = unsafe {
        if disable {
            // PERSIST is the point: the standby monitor's hot-plug RE-ARRIVAL must stay disabled,
            // otherwise every wake event recreates an enabled devnode and the churn is back.
            CM_Disable_DevNode(devinst, CM_DISABLE_PERSIST)
        } else {
            CM_Enable_DevNode(devinst, 0)
        }
    };
    if cr != CR_SUCCESS {
        tracing::warn!(
            id,
            cr = cr.0,
            disable,
            "PnP-disable: CM_{}_DevNode failed",
            if disable { "Disable" } else { "Enable" }
        );
        return false;
    }
    true
}

/// Disable the devnodes of every monitor the `Exclusive` isolate deactivated: all ACTIVE paths in
/// the pre-isolate snapshot whose target is not `keep_target_id`. Journals BEFORE disabling.
/// Returns the instance ids to re-enable at teardown.
pub fn disable_for_deactivated(
    saved: &crate::win_display::SavedConfig,
    keep_target_id: u32,
) -> Vec<String> {
    const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;
    let mut targets: Vec<(String, String)> = Vec::new();
    for p in &saved.0 {
        if p.targetInfo.id == keep_target_id || p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        match monitor_instance(p.targetInfo.adapterId, p.targetInfo.id) {
            Some(hit) => {
                if !targets.contains(&hit) {
                    targets.push(hit);
                }
            }
            None => tracing::debug!(
                target_id = p.targetInfo.id,
                "PnP-disable: no monitor device name for deactivated target — skipping"
            ),
        }
    }
    journal_and_disable(targets)
}

/// Disable the devnodes of every EXTERNAL PHYSICAL monitor that is connected but NOT part of the
/// desktop — regardless of who deactivated it. This is the standby-TV case the deactivated-set
/// selection above structurally misses: a TV that was never active has no pre-isolate active path,
/// yet its standby wake events (auto input scan, Instant-On HPD cycling) drive the same Windows
/// reaction cascade. Selection stays allowlist-precise via
/// [`crate::win_display::TargetInventory::external_physical`] — internal panels and
/// indirect/virtual targets (ours or third-party) can never be picked, and `keep_target_ids`
/// (the managed virtual set) is excluded belt-and-braces. Runs AFTER the topology action so the
/// active flags it reads are the settled ones. Journals like [`disable_for_deactivated`]; the
/// caller merges the returned ids into the same teardown list.
pub fn disable_connected_inactive(keep_target_ids: &[u32]) -> Vec<String> {
    let inventory = crate::win_display::target_inventory();
    let mut targets: Vec<(String, String)> = Vec::new();
    for t in &inventory {
        if t.active || !t.external_physical || keep_target_ids.contains(&t.target_id) {
            continue;
        }
        let Some(id) = instance_id_from_interface_path(&t.monitor_device_path) else {
            continue;
        };
        let hit = (id, format!("{} ({})", t.friendly, t.tech));
        if !targets.contains(&hit) {
            targets.push(hit);
        }
    }
    journal_and_disable(targets)
}

/// Shared tail of the two selectors: crash-journal FIRST, then disable, returning what actually
/// got disabled (the teardown re-enable list).
fn journal_and_disable(targets: Vec<(String, String)>) -> Vec<String> {
    if targets.is_empty() {
        tracing::debug!("PnP-disable: no physical monitor devnodes to disable");
        return Vec::new();
    }
    // Journal FIRST (union with any outstanding ids), so a crash between here and the disable
    // over-recovers instead of leaking a disabled monitor.
    let mut journal = read_journal();
    for (id, _) in &targets {
        if !journal.contains(id) {
            journal.push(id.clone());
        }
    }
    write_journal(&journal);
    let mut disabled = Vec::new();
    for (id, name) in targets {
        if set_devnode(&id, true) {
            tracing::info!(id, monitor = name, "PnP-disable: monitor devnode disabled");
            disabled.push(id);
        }
    }
    disabled
}

/// Re-enable `ids` (teardown / recovery) and clear the ones that actually re-enabled from the
/// journal. A FAILED re-enable must keep its journal entry: it is the only record that the
/// devnode is still disabled, and the next host start's [`startup_recover`] is the only thing
/// left that will retry it. (The old behavior cleared every requested id unconditionally — a
/// mid-life re-enable failure erased its own crash-recovery entry, leaving the operator's
/// monitor invisible to Windows AND to every display listing until they re-enabled it by hand
/// in Device Manager: the "my displays are gone until I restart everything" field class.)
pub fn enable_instances(ids: &[String]) -> u32 {
    let mut ok = 0u32;
    let mut reenabled: Vec<&String> = Vec::with_capacity(ids.len());
    for id in ids {
        if set_devnode(id, false) {
            tracing::info!(id, "PnP-disable: monitor devnode re-enabled");
            reenabled.push(id);
            ok += 1;
        } else {
            tracing::warn!(
                id,
                "PnP-disable: monitor devnode re-enable FAILED — keeping its crash-journal \
                 entry so the next host start retries (until then this monitor stays disabled)"
            );
        }
    }
    let journal: Vec<String> = read_journal()
        .into_iter()
        .filter(|j| !reenabled.contains(&j))
        .collect();
    write_journal(&journal);
    ok
}

/// Host-startup crash recovery: re-enable any devnodes a previous host disabled but never
/// restored (crash, kill, power loss). Call once, early in `serve`.
pub fn startup_recover() {
    let leftovers = read_journal();
    if leftovers.is_empty() {
        return;
    }
    tracing::warn!(
        count = leftovers.len(),
        "PnP-disable: found monitor devnodes a previous host left disabled — re-enabling"
    );
    enable_instances(&leftovers);
}
