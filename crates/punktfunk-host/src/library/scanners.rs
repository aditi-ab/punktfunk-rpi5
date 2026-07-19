//! Library-scanner settings: which installed-store scanners run on this host. Every scanner is
//! **on by default** (the shipped behavior before this existed); the operator can turn one off in
//! the web console, which hides its titles from every library surface (console grid, native
//! clients, the GameStream app list, launch resolution) from the next read. Only the *disabled*
//! set is persisted, so a scanner added in a future build starts enabled without a migration.
//!
//! The user-curated **custom** store is not a scanner (nothing is scanned — the operator typed the
//! entries in) and cannot be disabled here; provider plugins (RFC §8) likewise own their entries
//! through the reconcile API. Down the road the scanners themselves are slated to become plugins —
//! the stable per-scanner ids this module fixes (`steam`, `lutris`, …, matching each entry's
//! `store` field) are the forward seam for that migration.

use super::*;

/// One installed-store scanner this host build supports, with its enable state — the unit the
/// console renders a toggle for. The list is platform-gated at compile time (the scanners are),
/// so the console never shows a toggle that cannot do anything on this host.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ScannerInfo {
    /// Stable scanner id — the same string the scanner's entries carry in their `store` field.
    #[schema(example = "steam")]
    pub id: String,
    /// Human-facing name for the console toggle.
    #[schema(example = "Steam")]
    pub label: String,
    /// Whether this host runs the scanner (default true).
    pub enabled: bool,
}

/// The scanners compiled into THIS host build: (id, label). Steam is cross-platform; the rest are
/// platform-gated exactly like their provider modules in `library.rs` — keep the two in sync when
/// adding a store.
fn scanner_defs() -> Vec<(&'static str, &'static str)> {
    let mut defs = vec![("steam", "Steam")];
    #[cfg(target_os = "linux")]
    {
        defs.push(("lutris", "Lutris"));
        defs.push(("heroic", "Heroic (Epic / GOG / Amazon)"));
    }
    #[cfg(windows)]
    {
        defs.push(("epic", "Epic Games Launcher"));
        defs.push(("gog", "GOG Galaxy"));
        defs.push(("xbox", "Xbox / Game Pass"));
    }
    defs
}

/// Persisted shape (`library-scanners.json`): only the ids the operator turned OFF. Absent file =
/// nothing disabled = the pre-existing all-scanners-on behavior.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ScannerSettings {
    #[serde(default)]
    disabled: Vec<String>,
}

fn settings_path() -> PathBuf {
    // Same hardened config dir as library.json / hooks.json (see `custom_path` for the rationale).
    pf_paths::config_dir().join("library-scanners.json")
}

/// Load the settings (default + non-fatal if the file is absent or malformed).
fn load_settings() -> ScannerSettings {
    match std::fs::read_to_string(settings_path()) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "library-scanners.json malformed — all scanners on");
            ScannerSettings::default()
        }),
        Err(_) => ScannerSettings::default(),
    }
}

fn save_settings(settings: &ScannerSettings) -> Result<()> {
    let dir = pf_paths::config_dir();
    pf_paths::create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(settings)?;
    // Write-then-rename like the catalog, so a crash mid-write never truncates the settings.
    let tmp = settings_path().with_extension("json.tmp");
    pf_paths::write_secret_file(&tmp, json.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, settings_path()).context("rename library-scanners.json")?;
    Ok(())
}

/// The disabled-scanner ids, loaded once per library read ([`all_games`] consults it per store).
pub(crate) fn disabled_scanners() -> HashSet<String> {
    load_settings().disabled.into_iter().collect()
}

/// The scanners available on this platform with their current enable state, in the fixed
/// definition order (stable for the console).
pub fn list_scanners() -> Vec<ScannerInfo> {
    let off = disabled_scanners();
    scanner_defs()
        .into_iter()
        .map(|(id, label)| ScannerInfo {
            id: id.to_string(),
            label: label.to_string(),
            enabled: !off.contains(id),
        })
        .collect()
}

/// Enable/disable one scanner. `None` when `id` names no scanner available on this platform (the
/// mgmt layer maps that to 404 — the console only ever sees this host's own list). Persists and
/// emits `library.changed` (source = the scanner id) only when the state actually changed, so a
/// repeated PUT is a cheap no-op.
pub fn set_scanner_enabled(id: &str, enabled: bool) -> Result<Option<Vec<ScannerInfo>>> {
    if !scanner_defs().iter().any(|(sid, _)| *sid == id) {
        return Ok(None);
    }
    let mut settings = load_settings();
    let was_disabled = settings.disabled.iter().any(|d| d == id);
    if enabled != was_disabled {
        return Ok(Some(list_scanners()));
    }
    if enabled {
        settings.disabled.retain(|d| d != id);
    } else {
        settings.disabled.push(id.to_string());
        settings.disabled.sort();
        settings.disabled.dedup();
    }
    save_settings(&settings)?;
    crate::events::emit(crate::events::EventKind::LibraryChanged {
        source: id.to_string(),
    });
    Ok(Some(list_scanners()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_is_always_a_scanner_and_ids_are_unique() {
        let defs = scanner_defs();
        assert!(defs.iter().any(|(id, _)| *id == "steam"));
        let ids: HashSet<_> = defs.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), defs.len(), "scanner ids must be unique");
        // `custom` is a store but never a scanner — the toggle surface must not offer it.
        assert!(!ids.contains("custom"));
    }

    #[test]
    fn absent_or_malformed_settings_mean_all_on() {
        // The default (absent-file) settings disable nothing — the pre-feature behavior.
        let s = ScannerSettings::default();
        assert!(s.disabled.is_empty());
        let parsed: ScannerSettings = serde_json::from_str("{}").unwrap();
        assert!(parsed.disabled.is_empty(), "missing key defaults to empty");
    }
}
