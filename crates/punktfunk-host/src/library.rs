//! Game library (plan: "surface the user's games"). A small adapter layer over the *stores*
//! installed on the host — today **Steam** (read from local files, no API key) and a
//! user-curated **custom** store (CRUD'd via the management API / web console). Every store
//! produces the same [`GameEntry`], so a client renders one uniform grid and never has to know
//! which launcher a title came from. Future stores (Heroic/Epic, GOG, Lutris, EmuDeck) are just
//! more [`LibraryProvider`]s.
//!
//! Artwork is keyed only by Steam appid against the public Steam CDN (no auth) — the client
//! fetches the posters directly. Custom entries carry user-supplied art URLs.
//!
//! This module is read-mostly metadata; *launching* a chosen title (mapping [`LaunchSpec`] onto a
//! gamescope session) is a later step — the launch hint is carried here so that wiring is trivial.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use utoipa::ToSchema;

/// Cover art for a title. All fields are URLs (the Steam CDN for Steam titles, user-supplied for
/// custom). The client prefers `portrait` for a grid and falls back to `header` when a title has
/// no 600×900 capsule (common for older Steam apps).
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct Artwork {
    /// Vertical capsule / poster (Steam `library_600x900.jpg`). Best for a grid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portrait: Option<String>,
    /// Wide background (Steam `library_hero.jpg`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hero: Option<String>,
    /// Transparent title logo (Steam `logo.png`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// Horizontal header (Steam `header.jpg`) — the universal fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

/// How the host would launch a title (consumed by the session launcher in a later step). Kept
/// open-ended so new stores slot in: `steam_appid` → `steam steam://rungameid/<value>`;
/// `command` → run `<value>` nested in a gamescope session.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct LaunchSpec {
    /// `"steam_appid"` or `"command"`.
    #[schema(example = "steam_appid")]
    pub kind: String,
    /// The appid (for `steam_appid`) or the shell command (for `command`).
    pub value: String,
}

/// One title in the unified library, regardless of which store it came from.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct GameEntry {
    /// Stable, store-qualified id: `steam:<appid>` or `custom:<id>`.
    #[schema(example = "steam:570")]
    pub id: String,
    /// Which store surfaced it: `"steam"` or `"custom"`.
    #[schema(example = "steam")]
    pub store: String,
    pub title: String,
    pub art: Artwork,
    /// How the host would launch it, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchSpec>,
}

/// A store that contributes titles to the library. The trait is the extension point for future
/// launchers; today only [`SteamProvider`] implements it.
pub trait LibraryProvider {
    /// Stable store id (`"steam"`, …).
    fn store(&self) -> &'static str;
    /// Enumerate installed/owned titles. Best-effort: returns empty (not an error) when the store
    /// isn't present, so one missing launcher never fails the whole library.
    fn list(&self) -> Vec<GameEntry>;
}

// ---------------------------------------------------------------------------------------
// Steam
// ---------------------------------------------------------------------------------------

/// Reads the **local** Steam install — no Steam Web API key, no network. Installed titles come
/// from `steamapps/appmanifest_<appid>.acf`; extra library folders from
/// `steamapps/libraryfolders.vdf`; artwork from the public Steam CDN by appid.
pub struct SteamProvider;

impl LibraryProvider for SteamProvider {
    fn store(&self) -> &'static str {
        "steam"
    }

    fn list(&self) -> Vec<GameEntry> {
        let mut by_appid: std::collections::BTreeMap<u32, String> = Default::default();
        for steamapps in steam_library_dirs() {
            for (appid, name) in scan_manifests(&steamapps) {
                by_appid.entry(appid).or_insert(name); // first library wins; dedups shared appids
            }
        }
        by_appid
            .into_iter()
            .filter(|(appid, name)| !is_steam_tool(*appid, name))
            .map(|(appid, title)| GameEntry {
                id: format!("steam:{appid}"),
                store: "steam".into(),
                title,
                art: steam_art(appid),
                launch: Some(LaunchSpec {
                    kind: "steam_appid".into(),
                    value: appid.to_string(),
                }),
            })
            .collect()
    }
}

/// The Steam CDN poster/hero/logo/header for an appid (public, no auth). Not every appid has a
/// 600×900 capsule, but `header.jpg` is effectively universal — the client falls back to it.
fn steam_art(appid: u32) -> Artwork {
    let base = format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}");
    Artwork {
        portrait: Some(format!("{base}/library_600x900.jpg")),
        hero: Some(format!("{base}/library_hero.jpg")),
        logo: Some(format!("{base}/logo.png")),
        header: Some(format!("{base}/header.jpg")),
    }
}

/// Candidate Steam roots (classic, Flatpak, Deck) that actually exist, canonicalized + deduped.
fn steam_roots() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let candidates = [
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".steam/root"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"), // Flatpak Steam
    ];
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for c in candidates {
        if let Ok(canon) = c.canonicalize() {
            if canon.join("steamapps").is_dir() && seen.insert(canon.clone()) {
                roots.push(canon);
            }
        }
    }
    roots
}

/// Every `steamapps` dir holding installed titles: each root's own, plus the extra library
/// folders listed in `libraryfolders.vdf` (Steam lets you install games on other drives).
fn steam_library_dirs() -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    let mut push = |steamapps: PathBuf, dirs: &mut Vec<PathBuf>| {
        if let Ok(canon) = steamapps.canonicalize() {
            if canon.is_dir() && seen.insert(canon.clone()) {
                dirs.push(canon);
            }
        }
    };
    for root in steam_roots() {
        let steamapps = root.join("steamapps");
        if let Ok(text) = std::fs::read_to_string(steamapps.join("libraryfolders.vdf")) {
            for path in vdf_paths(&text) {
                push(PathBuf::from(path).join("steamapps"), &mut dirs);
            }
        }
        push(steamapps, &mut dirs);
    }
    dirs
}

/// Pull every `"path"  "<dir>"` value out of a `libraryfolders.vdf`. We don't need a full VDF
/// parser for the two flat fields we read — Linux library paths never contain the `"` or `\`
/// that would require unescaping.
fn vdf_paths(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| vdf_value(l.trim(), "path"))
        .map(str::to_string)
        .collect()
}

/// `"<key>" "<value>"` on a single line → `<value>`. Used for both VDF and ACF flat fields.
fn vdf_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(&format!("\"{key}\""))?;
    let after = &rest[rest.find('"')? + 1..];
    Some(&after[..after.find('"')?])
}

/// Scan a `steamapps` dir for `appmanifest_*.acf` files → (appid, name) of installed titles.
fn scan_manifests(steamapps: &Path) -> Vec<(u32, String)> {
    let Ok(rd) = std::fs::read_dir(steamapps) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let fname = entry.file_name();
        let fname = fname.to_string_lossy();
        if !(fname.starts_with("appmanifest_") && fname.ends_with(".acf")) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            let appid = text.lines().find_map(|l| vdf_value(l.trim(), "appid"));
            let name = text.lines().find_map(|l| vdf_value(l.trim(), "name"));
            if let (Some(Ok(appid)), Some(name)) = (appid.map(str::parse::<u32>), name) {
                out.push((appid, name.to_string()));
            }
        }
    }
    out
}

/// Steam installs runtimes/redistributables as "apps" too — keep them out of a *game* library.
fn is_steam_tool(appid: u32, name: &str) -> bool {
    // Steamworks Common Redistributables; Steam Linux Runtime 1.0/2.0/3.0 (Sniper/Soldier).
    const TOOL_IDS: &[u32] = &[228980, 1070560, 1391110, 1628350, 1493710];
    if TOOL_IDS.contains(&appid) {
        return true;
    }
    let n = name.to_ascii_lowercase();
    n.contains("proton")
        || n.starts_with("steam linux runtime")
        || n.contains("steamworks common")
        || n.contains("steamvr")
}

// ---------------------------------------------------------------------------------------
// Custom store (user-curated entries, persisted + CRUD'd via the mgmt API)
// ---------------------------------------------------------------------------------------

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
    Ok(true)
}

// ---------------------------------------------------------------------------------------
// Unified library
// ---------------------------------------------------------------------------------------

/// The full library: every store's titles merged + the custom entries, sorted by title.
pub fn all_games() -> Vec<GameEntry> {
    let mut games = SteamProvider.list();
    games.extend(load_custom().into_iter().map(GameEntry::from));
    games.sort_by_key(|g| g.title.to_lowercase());
    games
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vdf_value_extracts_quoted_field() {
        assert_eq!(
            vdf_value("\"path\"\t\t\"/mnt/games/SteamLibrary\"", "path"),
            Some("/mnt/games/SteamLibrary")
        );
        assert_eq!(vdf_value("\"appid\"\t\t\"570\"", "appid"), Some("570"));
        assert_eq!(vdf_value("\"name\"\t\t\"Dota 2\"", "name"), Some("Dota 2"));
        assert_eq!(vdf_value("\"installdir\"\t\t\"x\"", "appid"), None);
    }

    #[test]
    fn vdf_paths_pulls_all_library_folders() {
        let vdf = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"/home/u/.local/share/Steam"
		"apps" { "570" "123" }
	}
	"1"
	{
		"path"		"/mnt/ssd/SteamLibrary"
	}
}
"#;
        assert_eq!(
            vdf_paths(vdf),
            vec![
                "/home/u/.local/share/Steam".to_string(),
                "/mnt/ssd/SteamLibrary".to_string()
            ]
        );
    }

    #[test]
    fn tools_are_filtered_but_games_kept() {
        assert!(is_steam_tool(228980, "Steamworks Common Redistributables"));
        assert!(is_steam_tool(1493710, "Proton Experimental"));
        assert!(is_steam_tool(0, "Steam Linux Runtime 3.0 (sniper)"));
        assert!(!is_steam_tool(570, "Dota 2"));
        assert!(!is_steam_tool(1245620, "ELDEN RING"));
    }

    #[test]
    fn steam_art_uses_cdn_by_appid() {
        let art = steam_art(570);
        assert_eq!(
            art.portrait.as_deref(),
            Some("https://cdn.cloudflare.steamstatic.com/steam/apps/570/library_600x900.jpg")
        );
        assert!(art.header.unwrap().ends_with("/570/header.jpg"));
    }

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
