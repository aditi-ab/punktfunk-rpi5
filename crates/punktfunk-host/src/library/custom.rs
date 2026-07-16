//! User-curated custom store: CRUD (add/update/delete) over the persisted custom entries the web
//! console manages, and their mapping onto the uniform `GameEntry`. Split out of the `library` facade (plan §W5).

use super::*;

/// A user-added title, persisted in `~/.config/punktfunk/library.json`. Same shape the API
/// returns and the web console edits.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct CustomEntry {
    /// Host-assigned, stable for the life of the entry (the `{id}` in the CRUD path).
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchSpec>,
}

/// Request body to create or replace a custom entry (no `id` — the host owns it).
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct CustomInput {
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
    #[serde(default)]
    pub launch: Option<LaunchSpec>,
}

impl From<CustomEntry> for GameEntry {
    fn from(c: CustomEntry) -> Self {
        GameEntry {
            id: format!("custom:{}", c.id),
            store: "custom".into(),
            title: c.title,
            art: c.art,
            launch: c.launch,
        }
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("punktfunk")
}

fn custom_path() -> PathBuf {
    config_dir().join("library.json")
}

/// Load the custom entries (empty + non-fatal if the file is absent or malformed).
pub fn load_custom() -> Vec<CustomEntry> {
    match std::fs::read_to_string(custom_path()) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "library.json malformed — ignoring custom entries");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

fn save_custom(entries: &[CustomEntry]) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(entries)?;
    // Write-then-rename so a crash mid-write never truncates the catalog.
    let tmp = custom_path().with_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, custom_path()).context("rename library.json")?;
    Ok(())
}

/// 12 hex chars from the title + wall-clock nanos — collision-free in practice, no uuid dep.
fn new_id(title: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hex::encode(&Sha256::digest(format!("{title}:{nanos}").as_bytes())[..6])
}

/// Create a custom entry, returning it with its assigned id.
pub fn add_custom(input: CustomInput) -> Result<CustomEntry> {
    let mut entries = load_custom();
    let entry = CustomEntry {
        id: new_id(&input.title),
        title: input.title,
        art: input.art,
        launch: input.launch,
    };
    entries.push(entry.clone());
    save_custom(&entries)?;
    emit_changed();
    Ok(entry)
}

/// Replace a custom entry's fields (id preserved). `None` ⇒ no entry with that id.
pub fn update_custom(id: &str, input: CustomInput) -> Result<Option<CustomEntry>> {
    let mut entries = load_custom();
    let Some(slot) = entries.iter_mut().find(|e| e.id == id) else {
        return Ok(None);
    };
    slot.title = input.title;
    slot.art = input.art;
    slot.launch = input.launch;
    let updated = slot.clone();
    save_custom(&entries)?;
    emit_changed();
    Ok(Some(updated))
}

/// Delete a custom entry. `false` ⇒ no entry with that id.
pub fn delete_custom(id: &str) -> Result<bool> {
    let mut entries = load_custom();
    let before = entries.len();
    entries.retain(|e| e.id != id);
    if entries.len() == before {
        return Ok(false);
    }
    save_custom(&entries)?;
    emit_changed();
    Ok(true)
}

/// The custom-entry mutations are the only library writes today, all operator-driven — hence
/// `source: "manual"` (RFC §4; a provider id once the provider API of RFC §8 lands).
fn emit_changed() {
    crate::events::emit(crate::events::EventKind::LibraryChanged {
        source: "manual".to_string(),
    });
}

/// A digits-only Steam appid: the sole client-influenced part of a Steam launch, validated before it
/// is interpolated into any command / URI (so a client-sent id can never carry shell or URI syntax).
/// Cross-platform — used by the Linux shell mapping ([`command_for`]) and the Windows spawn mapping
/// ([`windows_launch_for`]).
pub(crate) fn valid_steam_appid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_entry_maps_to_game_entry() {
        let g: GameEntry = CustomEntry {
            id: "abc123".into(),
            title: "My ROM".into(),
            art: Artwork::default(),
            launch: None,
        }
        .into();
        assert_eq!(g.id, "custom:abc123");
        assert_eq!(g.store, "custom");
    }
}
