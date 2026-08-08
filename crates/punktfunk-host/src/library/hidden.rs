//! Per-entry visibility: the operator hides one *title*, where `scanners.rs` hides a whole source.
//!
//! **Why this is a side table and not a field on the entry.** Only manual custom entries are stored;
//! a scanner's and a plugin's titles are regenerated from scratch on every scan and every reconcile.
//! A `hidden` flag written onto one of those would be erased by the next sync — silently, and
//! minutes later, which is the worst possible shape for a setting. So the operator's choice lives
//! here, keyed by the entry's stable `<store>:<external_id>` id, and the entries stay disposable.
//!
//! That id is stable *by construction* (design D2): a claimed store's entries keep
//! `<store>:<external_id>` across reconciles no matter what the host-assigned id does, which is the
//! same property GameStream app ids and client art caches already depend on. Hiding therefore
//! survives a re-scan, a plugin restart, and the built-in→plugin migration for a store.
//!
//! Hiding is **curation, not access control** — it declutters a grid. It is applied in
//! [`all_games`](crate::library::all_games), so a hidden title is gone from every play surface
//! *including* launch resolution (the same reach a disabled scanner has), but nothing is deleted and
//! un-hiding is immediate. The console is the one surface that still sees hidden titles — otherwise
//! there would be no way to un-hide one — and only on the operator's own lane.

use super::*;

/// Persisted shape (`library-hidden.json`): the ids the operator hid. Absent file = nothing hidden.
///
/// Mirrors `library-scanners.json`'s disabled-set rather than sharing it: that file answers "which
/// SOURCES run", this one answers "which TITLES show", and a source id (`steam`) and an entry id
/// (`steam:70`) are different namespaces. Keeping them apart means neither migration can corrupt the
/// other, and an operator reading either file sees one idea.
#[derive(Debug, Default, Serialize, Deserialize)]
struct HiddenSettings {
    #[serde(default)]
    hidden: Vec<String>,
}

fn settings_path() -> PathBuf {
    // Same hardened config dir as library.json / library-scanners.json.
    pf_paths::config_dir().join("library-hidden.json")
}

/// Load the hidden set (default + non-fatal if the file is absent or malformed).
///
/// A malformed file means "nothing hidden", never "hide everything": the failure mode of a bad parse
/// must be a library that shows too much, not one that looks empty and reads as data loss.
fn load_settings() -> HiddenSettings {
    match std::fs::read_to_string(settings_path()) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "library-hidden.json malformed — nothing hidden");
            HiddenSettings::default()
        }),
        Err(_) => HiddenSettings::default(),
    }
}

fn save_settings(settings: &HiddenSettings) -> Result<()> {
    let dir = pf_paths::config_dir();
    pf_paths::create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(settings)?;
    // Write-then-rename like the catalog, so a crash mid-write never truncates the settings.
    let tmp = settings_path().with_extension("json.tmp");
    pf_paths::write_secret_file(&tmp, json.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, settings_path()).context("rename library-hidden.json")?;
    Ok(())
}

/// The hidden entry ids, loaded once per library read.
pub(crate) fn hidden_ids() -> HashSet<String> {
    load_settings().hidden.into_iter().collect()
}

/// The store half of a library id (`steam:70` → `steam`), for the `library.changed` source.
///
/// Falls back to the whole id rather than an empty string: an id without a `:` is not a shape this
/// host produces, and naming it in the event beats emitting a blank source that matches no cache key.
fn store_of(id: &str) -> &str {
    id.split_once(':').map_or(id, |(store, _)| store)
}

/// Hide or un-hide one entry. Returns whether the entry is hidden **after** the call.
///
/// Idempotent, and deliberately not validated against the current library: an entry can be absent
/// right now for reasons that have nothing to do with the operator's intent — the launcher is closed,
/// a plugin has not finished its first sync, a disk is unmounted. Refusing to hide a title that is
/// temporarily missing, or silently dropping the choice when it comes back, would both be worse than
/// storing an id that currently matches nothing. Persists and emits `library.changed` only when the
/// state actually changed, so a repeated PUT is a cheap no-op.
pub fn set_entry_hidden(id: &str, hidden: bool) -> Result<bool> {
    let mut settings = load_settings();
    let was_hidden = settings.hidden.iter().any(|h| h == id);
    if was_hidden == hidden {
        return Ok(hidden);
    }
    if hidden {
        settings.hidden.push(id.to_string());
        settings.hidden.sort();
        settings.hidden.dedup();
    } else {
        settings.hidden.retain(|h| h != id);
    }
    save_settings(&settings)?;
    crate::events::emit(crate::events::EventKind::LibraryChanged {
        source: store_of(id).to_string(),
    });
    Ok(hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The event source is the STORE, not the whole id — that is the key every client cache and the
    /// console's query invalidation is grouped by.
    #[test]
    fn store_of_takes_the_prefix_and_tolerates_a_bare_id() {
        assert_eq!(store_of("steam:70"), "steam");
        assert_eq!(store_of("custom:abc"), "custom");
        // An external id may itself contain a colon (Heroic's `legendary:<hash>`): split on the
        // FIRST one, or the store would come back wrong for exactly the store that does this.
        assert_eq!(store_of("heroic:legendary:fc0b13b7"), "heroic");
        assert_eq!(store_of("weird-no-colon"), "weird-no-colon");
    }

    /// A malformed settings file must read as "nothing hidden". The inverse — treating a parse
    /// failure as "hide everything" — would present as a library that lost its games.
    #[test]
    fn malformed_settings_hide_nothing() {
        let s: HiddenSettings = serde_json::from_str("{ not json").unwrap_or_default();
        assert!(s.hidden.is_empty());
        let s: HiddenSettings = serde_json::from_str("{}").expect("an empty object is valid");
        assert!(s.hidden.is_empty(), "absent key means nothing hidden");
    }

    /// The persisted shape is the contract an operator may hand-edit — pin it.
    #[test]
    fn settings_roundtrip_the_documented_shape() {
        let s: HiddenSettings =
            serde_json::from_str(r#"{"hidden":["steam:70","lutris:4"]}"#).expect("parses");
        assert_eq!(s.hidden, vec!["steam:70", "lutris:4"]);
        let json = serde_json::to_string(&s).expect("serializes");
        assert_eq!(json, r#"{"hidden":["steam:70","lutris:4"]}"#);
    }
}
