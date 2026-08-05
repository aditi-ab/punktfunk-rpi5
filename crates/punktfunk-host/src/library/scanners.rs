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

/// One **game source** on this host, with its enable state — the unit the console renders a toggle
/// for. A source is either a scanner compiled into this build or a plugin that reconciles entries in
/// (WP2.6); the console treats them identically, which is what makes the extraction invisible.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ScannerInfo {
    /// Stable source id — the same string this source's entries carry in their `store` field. For a
    /// plugin source it is also its provider id and its store claim: one string, by construction, so
    /// a user's disabled state survives a built-in scanner being replaced by its plugin.
    #[schema(example = "steam")]
    pub id: String,
    /// Human-facing name for the console toggle.
    #[schema(example = "Steam")]
    pub label: String,
    /// Whether this host runs the source (default true).
    pub enabled: bool,
    /// Where the source comes from: `builtin` (a scanner in this host build) or `plugin`.
    #[schema(example = "builtin")]
    pub origin: SourceOrigin,
    /// The provider id backing a `plugin` source — absent for a built-in scanner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// How many entries this source currently contributes. `None` for a built-in scanner, whose
    /// count would mean walking every launcher's files just to render a toggle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<usize>,
}

/// Where a [`ScannerInfo`] comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SourceOrigin {
    /// A scanner compiled into this host build.
    Builtin,
    /// A plugin reconciling entries over the provider API.
    Plugin,
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

/// Every game source on this host with its current enable state (WP2.6):
///
/// 1. the built-in scanners this build compiled in, **minus** any whose store a plugin has claimed
///    (the plugin replaces it, so showing both would offer two toggles for one thing);
/// 2. the claimed stores themselves, as plugin sources;
/// 3. any other provider that has entries — the *emergent* case (rom-manager, playnite), which has
///    never had a toggle before and gets one for free here.
///
/// Built-ins keep their fixed definition order (stable for the console); plugin sources follow,
/// sorted by id.
pub fn list_scanners() -> Vec<ScannerInfo> {
    let off = disabled_scanners();
    let claims = crate::library::claimed_stores();
    let entries = crate::library::load_custom();

    let mut out: Vec<ScannerInfo> = scanner_defs()
        .into_iter()
        .filter(|(id, _)| !claims.contains_key(*id))
        .map(|(id, label)| ScannerInfo {
            id: id.to_string(),
            label: label.to_string(),
            enabled: !off.contains(id),
            origin: SourceOrigin::Builtin,
            provider: None,
            entries: None,
        })
        .collect();

    // A claimed store shows under the SCANNER's label where we know one, so the row a user has been
    // toggling for releases doesn't rename itself out from under them mid-migration.
    let label_for = |id: &str| {
        scanner_defs()
            .into_iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, label)| label.to_string())
            .unwrap_or_else(|| id.to_string())
    };

    let mut plugin_ids: Vec<(String, String)> = claims
        .iter()
        .map(|(store, provider)| (store.clone(), provider.clone()))
        .collect();
    // Emergent providers: any provider with entries that isn't already listed via a claim.
    for e in &entries {
        let Some(provider) = e.provider.as_deref() else {
            continue;
        };
        if e.store.is_none() && !plugin_ids.iter().any(|(id, _)| id == provider) {
            plugin_ids.push((provider.to_string(), provider.to_string()));
        }
    }
    plugin_ids.sort();
    plugin_ids.dedup();

    out.extend(plugin_ids.into_iter().map(|(id, provider)| {
        let count = entries
            .iter()
            .filter(|e| crate::library::source_id_for(e) == Some(id.as_str()))
            .count();
        ScannerInfo {
            label: label_for(&id),
            enabled: !off.contains(&id),
            origin: SourceOrigin::Plugin,
            provider: Some(provider),
            entries: Some(count),
            id,
        }
    }));
    out
}

/// Whether `id` names a source that exists on this host right now — a compiled-in scanner, a claimed
/// store, or a provider with entries. The toggle accepts exactly these (an unknown id still 404s).
fn is_known_source(id: &str) -> bool {
    scanner_defs().iter().any(|(sid, _)| *sid == id) || list_scanners().iter().any(|s| s.id == id)
}

/// Enable/disable one source. `None` when `id` names no source on this host (the mgmt layer maps
/// that to 404 — the console only ever sees this host's own list). Persists and emits
/// `library.changed` (source = the id) only when the state actually changed, so a repeated PUT is a
/// cheap no-op.
///
/// The **same** `library-scanners.json` disabled-set backs built-in and plugin sources alike, and
/// the ids match by construction — so a user who disabled `steam` before the migration still has it
/// disabled after the steam plugin claims the store, with nothing to carry over.
pub fn set_scanner_enabled(id: &str, enabled: bool) -> Result<Option<Vec<ScannerInfo>>> {
    if !is_known_source(id) {
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
