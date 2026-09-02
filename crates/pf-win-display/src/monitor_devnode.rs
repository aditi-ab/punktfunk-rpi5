//! PnP monitor-devnode disable. Two selectors, two defaults:
//! [`disable_connected_inactive`] (standby sinks — on by default; see
//! `pf_vdisplay::policy::standby_sink_neutralise`) and [`disable_for_deactivated`] (the operator's
//! own displays — experimental opt-in `pnp_disable_monitors`).
//!
//! An `Exclusive` isolate removes physical monitors from the CCD topology, but their PnP nodes
//! stay live, so a standby sink that wakes the link still drives PnP arrival/removal, CCD
//! re-evaluation, and DWM invalidation. This module disables those nodes for the stream
//! (`CM_Disable_DevNode` + `CM_DISABLE_PERSIST`, so a hot-plug re-arrival stays disabled) and
//! re-enables them at teardown before the CCD restore. Selectors are allowlists: third-party
//! virtual displays are never touched.
//!
//! Instance ids are journaled to `<config>/pnp-disabled-monitors.json` before the disable and
//! cleared after a successful re-enable. [`startup_recover`] re-enables leftovers on host start.
//! If the host dies and never restarts, the monitor stays disabled until Device Manager.

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

/// Crash-recovery journal of PnP instance ids disabled and not yet re-enabled.
fn journal_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("pnp-disabled-monitors.json")
}

fn read_journal() -> Vec<String> {
    match std::fs::read(journal_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Persist `ids` as the outstanding-disable set (union is the caller's job). Failure is logged, not
/// fatal — the feature degrades to "no crash journal", not "no feature".
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

// `display_events` applies the same transform to DBT_DEVICEARRIVAL interface paths.
pub fn instance_id_from_interface_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix(r"\\?\")?;
    let cut = rest.rfind("#{")?;
    Some(rest[..cut].replace('#', "\\"))
}

fn utf16z(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

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

fn set_devnode(id: &str, disable: bool) -> bool {
    let wide: Vec<u16> = id.encode_utf16().chain([0]).collect();
    let mut devinst = 0u32;
    // A disabled or departed devnode may not be in the live tree — PHANTOM on enable so recovery
    // still finds it; disable requires a present device.
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
            // PERSIST is the point: a standby monitor's hot-plug re-arrival must stay disabled.
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

/// Journals before disabling. Returns the instance ids to re-enable at teardown.
pub fn disable_for_deactivated(
    saved: &crate::win_display::SavedConfig,
    keep: crate::win_display::CcdTargetKey,
) -> Vec<String> {
    const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;
    let mut targets: Vec<(String, String)> = Vec::new();
    for p in &saved.0 {
        if crate::win_display::path_target_key(p) == keep
            || p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0
        {
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
/// `baseline_active` is the caller's PRE-MUTATION snapshot — the targets that were part of the
/// desktop BEFORE this acquire touched the topology. It is what tells a genuine standby sink
/// (inactive before us too) from an operator display the isolate itself just switched off: the
/// post-isolate inventory alone cannot (immunity plan WP3a — the selector used to run after the
/// Exclusive isolate and disable the operator's freshly-deactivated panel as a "sink").
pub fn disable_connected_inactive(
    keep: &[crate::win_display::CcdTargetKey],
    baseline_active: &[crate::win_display::CcdTargetKey],
) -> Vec<String> {
    let targets = select_connected_inactive(
        &crate::win_display::target_inventory(),
        keep,
        baseline_active,
    );
    journal_and_disable(targets)
}

/// The pure selection half of [`disable_connected_inactive`], split from the PnP mutation so the
/// baseline rule is testable without a live CCD.
fn select_connected_inactive(
    inventory: &[crate::win_display::TargetInventory],
    keep: &[crate::win_display::CcdTargetKey],
    baseline_active: &[crate::win_display::CcdTargetKey],
) -> Vec<(String, String)> {
    let mut targets: Vec<(String, String)> = Vec::new();
    for t in inventory {
        if t.active || !t.external_physical || keep.contains(&t.key) {
            continue;
        }
        // Active before this acquire touched the topology ⇒ an operator display we (or a racing
        // actor) just switched off — NEVER a standby sink. A target absent from the baseline was
        // dark before us (or arrived dark) and stays a sink candidate.
        if baseline_active.contains(&t.key) {
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
    targets
}

/// Crash-journal first, then disable; return what actually disabled (the teardown re-enable list).
fn journal_and_disable(targets: Vec<(String, String)>) -> Vec<String> {
    if targets.is_empty() {
        tracing::debug!("PnP-disable: no physical monitor devnodes to disable");
        return Vec::new();
    }
    // Journal first (union with outstanding ids): a crash between here and the disable
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

/// Re-enable `ids` and drop the ones that actually re-enabled from the journal. A failed re-enable
/// must keep its journal entry: that is the only record the devnode is still disabled, and the next
/// [`startup_recover`] is the only retry.
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

/// Re-enable leftover journaled devnodes from a previous host that crashed, was killed, or lost power.
/// Call once, early in `serve`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win_display::{CcdTargetKey, TargetInventory};

    fn target(luid: i64, id: u32, active: bool, external: bool) -> TargetInventory {
        TargetInventory {
            key: CcdTargetKey::new(luid, id),
            target_id: id,
            active,
            external_physical: external,
            internal_panel: false,
            tech: "HDMI",
            friendly: "ACME TV".into(),
            monitor_device_path: format!(r"\\?\DISPLAY#ACM{id:04}#5&1&0&UID{id}#{{guid}}"),
            ours: false,
            gdi_name: String::new(),
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            refresh_mhz: 0,
            primary: false,
        }
    }

    /// WP3a's whole point: an operator display that was ACTIVE before this acquire touched the
    /// topology is never devnode-disabled by the default standby-sink treatment, however
    /// inactive it reads after the isolate — while a genuinely pre-dark sink still is.
    #[test]
    fn a_display_active_before_the_acquire_is_never_a_standby_sink() {
        let keep = [CcdTargetKey::new(9, 257)]; // the virtual display
                                                // Post-isolate view: the operator's panel (100) AND the standby TV (200) both read
                                                // connected-but-inactive — indistinguishable without the baseline.
        let inventory = [
            target(1, 100, false, true), // operator panel, deactivated by OUR isolate
            target(1, 200, false, true), // standby TV, dark before us too
            target(9, 257, true, false), // ours
        ];
        let baseline_active = [CcdTargetKey::new(1, 100)];
        let picked = select_connected_inactive(&inventory, &keep, &baseline_active);
        assert_eq!(picked.len(), 1, "only the pre-dark sink is selected");
        assert!(picked[0].0.contains("ACM0200"), "picked: {picked:?}");
        // Same-numbered target on ANOTHER adapter must not shadow the baseline entry.
        let alias_baseline = [CcdTargetKey::new(2, 100)];
        let picked = select_connected_inactive(&inventory, &keep, &alias_baseline);
        assert_eq!(
            picked.len(),
            2,
            "adapter 1's target 100 was NOT in this baseline"
        );
    }
}
