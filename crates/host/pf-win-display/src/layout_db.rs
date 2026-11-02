//! Isolates this host stored in the Windows display database, until the operator's layout is back.
//!
//! Windows keeps one layout per connected monitor set and re-applies it whenever the set
//! re-resolves, as when a monitor re-plugs itself going into standby. The exclusive re-assert
//! stores the isolate so that re-plug keeps it, and teardown stores the operator's layout back.
//! This journal names every set whose stored layout still keeps the operator's panels dark, so a
//! host that died first lights them the next time that set is connected. The set and relight
//! rules are pure and tested everywhere; the file and the CCD writes are Windows-only.

use crate::snapshot::{CcdTargetKey, TargetInventory};

/// One stored isolate. Device paths are lowercased: the OS does not keep their case stable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    /// Every connected target's device path, ours included, sorted: the database's key.
    pub set: Vec<String>,
    /// The operator's panels the stored layout keeps dark.
    pub dark: Vec<String>,
}

/// The database key of the topology `inv` describes.
pub fn connected_set(inv: &[TargetInventory]) -> Vec<String> {
    let mut set: Vec<String> = inv
        .iter()
        .map(|t| t.monitor_device_path.to_lowercase())
        .collect();
    set.sort();
    set.dedup();
    set
}

/// Merge `entry` into `journal`. `false` when the journal already covered it.
pub fn record(journal: &mut Vec<Stored>, entry: Stored) -> bool {
    let Some(have) = journal.iter_mut().find(|s| s.set == entry.set) else {
        journal.push(entry);
        return true;
    };
    let before = have.dark.len();
    for d in entry.dark {
        if !have.dark.contains(&d) {
            have.dark.push(d);
        }
    }
    have.dark.len() != before
}

/// The panels to light on the topology `inv` describes: its set's journaled dark panels that are
/// connected and inactive. `None` when the journal holds no isolate for this set.
pub fn to_relight(journal: &[Stored], inv: &[TargetInventory]) -> Option<Vec<CcdTargetKey>> {
    let set = connected_set(inv);
    let stored = journal.iter().find(|s| s.set == set)?;
    Some(
        inv.iter()
            .filter(|t| !t.active && stored.dark.contains(&t.monitor_device_path.to_lowercase()))
            .map(|t| t.key)
            .collect(),
    )
}

#[cfg(target_os = "windows")]
pub use imp::{forget_if_lit, holds_connected_set, mark, repair};

#[cfg(target_os = "windows")]
mod imp {
    use super::{connected_set, record, to_relight, Stored};
    use crate::snapshot::{CcdTargetKey, TargetInventory};

    use std::path::Path;

    fn path() -> std::path::PathBuf {
        pf_paths::config_dir().join("display-layouts-stored.json")
    }

    /// An unreadable journal reads as empty: there is nothing left to repair from.
    fn read(p: &Path) -> Vec<Stored> {
        let Ok(bytes) = std::fs::read(p) else {
            return Vec::new();
        };
        serde_json::from_slice::<Vec<(Vec<String>, Vec<String>)>>(&bytes)
            .unwrap_or_default()
            .into_iter()
            .map(|(set, dark)| Stored { set, dark })
            .collect()
    }

    fn write(p: &Path, journal: &[Stored]) -> bool {
        if journal.is_empty() {
            return std::fs::remove_file(p).is_ok() || !p.exists();
        }
        if let Some(dir) = p.parent() {
            let _ = pf_paths::create_private_dir(dir);
        }
        let rows: Vec<(&[String], &[String])> = journal
            .iter()
            .map(|s| (s.set.as_slice(), s.dark.as_slice()))
            .collect();
        match std::fs::write(p, serde_json::to_vec_pretty(&rows).unwrap_or_default()) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "stored-layout journal not written");
                false
            }
        }
    }

    /// Journal that an isolate about to be stored for the set `inv` describes keeps `darkened`
    /// dark. `false` when nothing was journaled: the caller must not store the isolate then.
    pub fn mark(inv: &[TargetInventory], darkened: &[CcdTargetKey]) -> bool {
        let dark: Vec<String> = inv
            .iter()
            .filter(|t| !t.ours && darkened.contains(&t.key))
            .map(|t| t.monitor_device_path.to_lowercase())
            .collect();
        if dark.is_empty() {
            return false;
        }
        let mut journal = read(&path());
        let entry = Stored {
            set: connected_set(inv),
            dark,
        };
        !record(&mut journal, entry) || write(&path(), &journal)
    }

    /// Whether the journal holds an isolate stored for the connected set.
    pub fn holds_connected_set() -> bool {
        let journal = read(&path());
        !journal.is_empty()
            && to_relight(&journal, &crate::win_display::target_inventory()).is_some()
    }

    /// Drop the connected set's entry once every panel it darkened is lit: then the layout
    /// Windows stores for the set is no longer the isolate.
    pub fn forget_if_lit() {
        let mut journal = read(&path());
        let inv = crate::win_display::target_inventory();
        if to_relight(&journal, &inv).is_none_or(|dark| !dark.is_empty()) {
            return;
        }
        let set = connected_set(&inv);
        journal.retain(|s| s.set != set);
        write(&path(), &journal);
    }

    /// A monitor joined the desktop. When the connected set came up from an isolate a dead host
    /// stored, light the panels it keeps dark; each light stores the new layout.
    pub fn repair() {
        let journal = read(&path());
        if journal.is_empty() {
            return;
        }
        let inv = crate::win_display::target_inventory();
        let Some(keys) = to_relight(&journal, &inv) else {
            return;
        };
        if !keys.is_empty() {
            let lit = keys
                .iter()
                .filter(|k| crate::win_display::activate_target_path(**k))
                .count();
            tracing::info!(
                dark = keys.len(),
                lit,
                "stored-isolate repair: this monitor set came up dark from an isolate a previous \
                 host stored; lit its panels"
            );
        }
        forget_if_lit();
    }

    /// A journal a dead host wrote is what the next host reads back; an empty one is no file.
    #[test]
    fn the_journal_survives_a_round_trip_through_its_file() {
        // Its own dir: `write` hardens the parent, which must not be the shared temp root.
        let dir = std::env::temp_dir().join(format!("pf-layout-db-{}", std::process::id()));
        let p = dir.join("roundtrip.json");
        let journal = vec![Stored {
            set: vec!["a".into(), "b".into()],
            dark: vec!["a".into()],
        }];
        assert!(write(&p, &journal));
        assert_eq!(read(&p), journal);
        assert!(write(&p, &[]));
        assert!(!p.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: u32, path: &str, active: bool, ours: bool) -> TargetInventory {
        TargetInventory {
            key: CcdTargetKey::new(0x1f, id),
            target_id: id,
            active,
            external_physical: !ours,
            internal_panel: false,
            tech: if ours { "punktfunk-virtual" } else { "HDMI" },
            friendly: String::new(),
            monitor_device_path: path.into(),
            ours,
            gdi_name: String::new(),
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            refresh_mhz: 0,
            primary: false,
            hdr: None,
            wcg: None,
            sdr_white_level: None,
            source_id: 0,
            source_adapter_luid: 0,
        }
    }

    const ASUS: &str = r"\\?\DISPLAY#AUSAA02#5&1332526&0&UID4352#{guid}";
    const OURS: &str = r"\\?\DISPLAY#PNK0000#5&1234abcd&0&UID259#{guid}";

    fn stored(dark: &[&str]) -> Stored {
        Stored {
            set: connected_set(&[t(1, ASUS, false, false), t(259, OURS, true, true)]),
            dark: dark.iter().map(|d| d.to_lowercase()).collect(),
        }
    }

    /// The key survives the OS changing a path's case or the query's order.
    #[test]
    fn the_set_ignores_case_and_order() {
        let a = connected_set(&[t(1, ASUS, true, false), t(259, OURS, true, true)]);
        let b = connected_set(&[
            t(259, &OURS.to_uppercase(), true, true),
            t(1, &ASUS.to_lowercase(), false, false),
        ]);
        assert_eq!(a, b);
    }

    /// Every re-assert round marks again; only a new panel is a new journal.
    #[test]
    fn recording_the_same_isolate_twice_changes_nothing() {
        let mut journal = Vec::new();
        assert!(record(&mut journal, stored(&[ASUS])));
        assert!(!record(&mut journal, stored(&[ASUS])));
        assert_eq!(journal.len(), 1);
    }

    /// After a host died: the set comes back with the panel dark, and only that panel lights.
    #[test]
    fn a_stored_set_relights_its_dark_panel() {
        let journal = vec![stored(&[ASUS])];
        let inv = [t(1, ASUS, false, false), t(259, OURS, true, true)];
        assert_eq!(
            to_relight(&journal, &inv),
            Some(vec![CcdTargetKey::new(0x1f, 1)])
        );
    }

    /// The set came up lit: Windows applied another layout, nothing to light.
    #[test]
    fn a_lit_panel_needs_no_light() {
        let journal = vec![stored(&[ASUS])];
        let inv = [t(1, ASUS, true, false), t(259, OURS, true, true)];
        assert_eq!(to_relight(&journal, &inv), Some(vec![]));
    }

    /// A different connected set is a different database key: leave it alone.
    #[test]
    fn another_set_is_not_repaired() {
        let journal = vec![stored(&[ASUS])];
        let tv = r"\\?\DISPLAY#GSM83CD#5&367fb4cb&0&UID4353#{guid}";
        let inv = [
            t(1, ASUS, false, false),
            t(2, tv, false, false),
            t(259, OURS, true, true),
        ];
        assert_eq!(to_relight(&journal, &inv), None);
    }
}
