//! Game-source settings: which of this host's library sources contribute titles. Every source is
//! **on by default**; the operator can turn one off in the web console, which hides its titles from
//! every library surface (console grid, native clients, the GameStream app list, launch resolution)
//! from the next read. Only the *disabled* set is persisted, so a source that appears later starts
//! enabled without a migration.
//!
//! **Every source here is a plugin now.** Through v0.27.x this module also enumerated the six
//! scanners compiled into the host; that list is gone with the scanners themselves. The forward seam
//! it was built for did its job exactly as designed — the ids never changed (provider id = claimed
//! store id = old scanner id), so an operator who had `steam` switched off before the migration
//! still has it switched off after, with nothing to carry over and no migration step. That property
//! is the reason `library-scanners.json` keeps its name and its shape.
//!
//! The user-curated **custom** store is not a source (nothing is scanned — the operator typed the
//! entries in) and cannot be disabled here.

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
    /// Where the source comes from. Always `plugin` from this host build onward — see
    /// [`SourceOrigin`].
    #[schema(example = "plugin")]
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
    /// A scanner compiled into the host build.
    ///
    /// **No host build emits this any more** — the built-in scanners were removed in v0.28.0. The
    /// variant is kept deliberately, because it is still part of the API's vocabulary: the web
    /// console ships as its own package and is expected to drive an N-1 host, which does still
    /// report `builtin` sources. Deleting it here would drop `builtin` from the OpenAPI enum and
    /// narrow the console's generated union out from under that pairing.
    #[allow(dead_code)]
    Builtin,
    /// A plugin reconciling entries over the provider API.
    Plugin,
}

/// Display names for the stores that used to have a built-in scanner, so a source keeps the label
/// the operator has been toggling for releases instead of renaming itself to a bare id the day its
/// plugin takes over. Anything not listed (rom-manager, playnite, a third-party provider) falls back
/// to its own id, which is what those sources have always shown.
const STORE_LABELS: &[(&str, &str)] = &[
    ("steam", "Steam"),
    ("lutris", "Lutris"),
    ("heroic", "Heroic (Epic / GOG / Amazon)"),
    ("epic", "Epic Games Launcher"),
    ("gog", "GOG Galaxy"),
    ("xbox", "Xbox / Game Pass"),
];

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

/// The disabled source ids, loaded once per library read ([`all_games`] filters each entry on it).
pub(crate) fn disabled_scanners() -> HashSet<String> {
    load_settings().disabled.into_iter().collect()
}

/// Every game source on this host with its current enable state (WP2.6):
///
/// 1. every **claimed store** — a library plugin that took a store's id, so its entries surface as
///    `steam:570` rather than `custom:<opaque>`;
/// 2. every other provider that has entries — the *emergent* case (rom-manager, playnite), which
///    never claimed a store but still owns a set of titles the operator may want to switch off.
///
/// Sorted by id, which is a stable order for the console. This used to lead with the built-in
/// scanners compiled into the host, minus any store a plugin had claimed out from under them; with
/// the scanners gone the subtraction has nothing left to subtract and the list is plugins only.
pub fn list_scanners() -> Vec<ScannerInfo> {
    let off = disabled_scanners();
    let claims = crate::library::claimed_stores();
    let entries = crate::library::load_custom();

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

    plugin_ids
        .into_iter()
        .map(|(id, provider)| {
            let count = entries
                .iter()
                .filter(|e| crate::library::source_id_for(e) == Some(id.as_str()))
                .count();
            ScannerInfo {
                label: store_label(&id),
                enabled: !off.contains(&id),
                origin: SourceOrigin::Plugin,
                provider: Some(provider),
                entries: Some(count),
                id,
            }
        })
        .collect()
}

/// A source's display name — see [`STORE_LABELS`]; its own id when we know no nicer name.
fn store_label(id: &str) -> String {
    STORE_LABELS
        .iter()
        .find(|(sid, _)| *sid == id)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| id.to_string())
}

/// Whether `id` names a source that exists on this host right now — a claimed store or a provider
/// with entries. The toggle accepts exactly these (an unknown id still 404s).
///
/// Note this is now strictly "a source that is really here". While the built-ins existed it also
/// accepted any compiled-in scanner id, which was the same thing for them; a plugin that has never
/// reconciled has no entries and no claim, so there is nothing to toggle and 404 is the honest
/// answer.
fn is_known_source(id: &str) -> bool {
    list_scanners().iter().any(|s| s.id == id)
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

    /// The label table is what keeps a source row named "Steam" instead of "steam" now that the
    /// scanner which used to supply that name is gone. Pin both halves: the ids are unique, and an
    /// id we know nothing about degrades to itself rather than to an empty or panicking label.
    #[test]
    fn store_labels_are_unique_and_unknown_ids_degrade_to_themselves() {
        let ids: HashSet<_> = STORE_LABELS.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), STORE_LABELS.len(), "source ids must be unique");
        // `custom` is a store but never a source — the toggle surface must not offer it.
        assert!(!ids.contains("custom"));

        assert_eq!(store_label("steam"), "Steam");
        assert_eq!(store_label("rom-manager"), "rom-manager");
        assert_eq!(store_label(""), "");
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
