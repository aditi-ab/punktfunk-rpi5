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
#[cfg(not(target_os = "windows"))]
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
    steam_roots_existing(candidates)
}

/// Windows Steam roots: the default install dirs under Program Files. Games installed on other
/// drives are still found via each root's `libraryfolders.vdf` (see [`steam_library_dirs`]). A
/// non-default Steam install dir (registry `Valve\Steam\InstallPath`) isn't covered yet.
#[cfg(target_os = "windows")]
fn steam_roots() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            candidates.push(PathBuf::from(pf).join("Steam"));
        }
    }
    steam_roots_existing(candidates)
}

/// Keep only the candidate roots that exist (have a `steamapps` dir), canonicalized + deduped.
fn steam_roots_existing(candidates: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
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
/// parser for the two flat fields we read. On Windows the values are backslash-escaped
/// (`D:\\SteamLibrary`), so unescape `\\` → `\`; Linux paths need no unescaping.
fn vdf_paths(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| vdf_value(l.trim(), "path"))
        .map(|p| {
            #[cfg(target_os = "windows")]
            {
                p.replace("\\\\", "\\")
            }
            #[cfg(not(target_os = "windows"))]
            {
                p.to_string()
            }
        })
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
// Lutris (Linux) — reads the local `pga.db` (no auth, no network). One provider covers
// everything Lutris manages: Wine/Proton games, GOG/Epic/Battle.net installs, emulators.
// ---------------------------------------------------------------------------------------

/// Reads the **local** Lutris library DB (`pga.db`) — no network. Installed titles only; cover art
/// from Lutris's on-disk cache, inlined as `data:` URLs. Linux-only (Lutris is Linux-only).
#[cfg(target_os = "linux")]
pub struct LutrisProvider;

#[cfg(target_os = "linux")]
impl LibraryProvider for LutrisProvider {
    fn store(&self) -> &'static str {
        "lutris"
    }

    fn list(&self) -> Vec<GameEntry> {
        let Some(db) = lutris_db() else {
            return Vec::new();
        };
        lutris_games(&db).unwrap_or_else(|e| {
            tracing::warn!(error = %e, db = %db.display(), "lutris pga.db read failed — skipping");
            Vec::new()
        })
    }
}

/// The first existing Lutris `pga.db`: XDG data dir, the classic `~/.local/share`, or Flatpak.
#[cfg(target_os = "linux")]
fn lutris_db() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        candidates.push(PathBuf::from(d).join("lutris/pga.db"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/share/lutris/pga.db"));
        candidates.push(home.join(".var/app/net.lutris.Lutris/data/lutris/pga.db"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Installed games from a Lutris `pga.db`. Opened **read-only + immutable** (via a SQLite URI) so a
/// running Lutris holding the file can't make us block or fail, and we never write to it.
#[cfg(target_os = "linux")]
fn lutris_games(db: &Path) -> rusqlite::Result<Vec<GameEntry>> {
    use rusqlite::OpenFlags;
    // `immutable=1` treats the DB as read-only-and-unchanging → no locking against a live Lutris. The
    // path goes into the URI literally; a `?`/`#` in it (vanishingly rare on Linux) would mis-parse,
    // so fall back to a plain read-only open in that case.
    let path = db.to_string_lossy();
    let conn = if path.contains('?') || path.contains('#') {
        rusqlite::Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?
    } else {
        rusqlite::Connection::open_with_flags(
            format!("file:{path}?immutable=1"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?
    };
    let mut stmt = conn.prepare(
        "SELECT id, slug, name FROM games \
         WHERE installed = 1 AND name IS NOT NULL AND name <> '' \
         ORDER BY name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut games = Vec::new();
    for (id, slug, name) in rows.flatten() {
        games.push(GameEntry {
            id: format!("lutris:{id}"),
            store: "lutris".into(),
            title: name,
            art: slug.as_deref().map(lutris_art).unwrap_or_default(),
            launch: Some(LaunchSpec {
                kind: "lutris_id".into(),
                value: id.to_string(),
            }),
        });
    }
    Ok(games)
}

/// Lutris cover art (local files keyed by slug) inlined as `data:` URLs — Lutris has no public CDN
/// keyed by a stable id (unlike Steam/Heroic), and `Artwork` fields are URLs the client fetches, so a
/// self-contained `data:` URL needs no host-served endpoint. `coverart` → portrait, `banners` → header.
#[cfg(target_os = "linux")]
fn lutris_art(slug: &str) -> Artwork {
    Artwork {
        portrait: lutris_image("coverart", slug),
        header: lutris_image("banners", slug),
        ..Default::default()
    }
}

/// Find `<kind>/<slug>.jpg` across the current (0.5.18+), legacy (`~/.cache`), and Flatpak Lutris
/// dirs and inline it as `data:image/jpeg;base64,…`. Skips a missing or implausibly large file (a
/// 1 MiB cap bounds the catalog JSON so a few big files can't bloat it).
#[cfg(target_os = "linux")]
fn lutris_image(kind: &str, slug: &str) -> Option<String> {
    use base64::Engine as _;
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let roots = [
        home.join(".local/share/lutris"),
        home.join(".cache/lutris"),
        home.join(".var/app/net.lutris.Lutris/data/lutris"),
        home.join(".var/app/net.lutris.Lutris/cache/lutris"),
    ];
    for root in roots {
        let p = root.join(kind).join(format!("{slug}.jpg"));
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.len() == 0 || meta.len() > 1024 * 1024 {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&p) {
            let enc = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Some(format!("data:image/jpeg;base64,{enc}"));
        }
    }
    None
}

// ---------------------------------------------------------------------------------------
// Heroic (Linux) — Epic + GOG + Amazon in one provider. Reads Heroic's `store_cache` JSON
// (no auth); cover art is already public Epic/GOG/Amazon CDN URLs the client fetches directly.
// ---------------------------------------------------------------------------------------

/// Reads Heroic Games Launcher's local library cache. One provider surfaces all three of Heroic's
/// backends (legendary=Epic, gog=GOG, nile=Amazon). Linux-only for now (Heroic on Windows uses a
/// different config path and the launch path isn't wired there yet).
#[cfg(target_os = "linux")]
pub struct HeroicProvider;

#[cfg(target_os = "linux")]
impl LibraryProvider for HeroicProvider {
    fn store(&self) -> &'static str {
        "heroic"
    }

    fn list(&self) -> Vec<GameEntry> {
        let Some(root) = heroic_root() else {
            return Vec::new();
        };
        let mut games = Vec::new();
        // (cache file, runner id, the electron-store data key holding the games array)
        for (file, runner, key) in [
            ("legendary_library.json", "legendary", "library"),
            ("gog_library.json", "gog", "games"),
            ("nile_library.json", "nile", "library"),
        ] {
            let path = root.join("store_cache").join(file);
            match heroic_games(&path, runner, key) {
                Ok(mut g) => games.append(&mut g),
                Err(e) => {
                    tracing::debug!(error = %e, file, "heroic store_cache not read (store unused?)")
                }
            }
        }
        games
    }
}

/// The first existing Heroic config root: `$XDG_CONFIG_HOME/heroic`, classic `~/.config/heroic`, or
/// the Flatpak path.
#[cfg(target_os = "linux")]
fn heroic_root() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        candidates.push(PathBuf::from(d).join("heroic"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".config/heroic"));
        candidates.push(home.join(".var/app/com.heroicgameslauncher.hgl/config/heroic"));
    }
    candidates.into_iter().find(|p| p.is_dir())
}

/// Parse one runner's `store_cache/*_library.json` (an electron-store object whose `key` holds the
/// games array). Keeps only installed titles whose install dir still exists (the latter works around
/// Heroic's gog `is_installed` bug, #2691). Art comes straight from the cached public CDN URLs.
#[cfg(target_os = "linux")]
fn heroic_games(path: &Path, runner: &str, key: &str) -> anyhow::Result<Vec<GameEntry>> {
    let raw = std::fs::read_to_string(path)?;
    let root: serde_json::Value = serde_json::from_str(&raw)?;
    let arr = root
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("no '{key}' array in {}", path.display()))?;
    let mut games = Vec::new();
    for g in arr {
        if !g
            .get("is_installed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue; // the cache also lists owned-but-not-installed titles
        }
        let install_ok = g
            .get("install")
            .and_then(|i| i.get("install_path"))
            .and_then(|p| p.as_str())
            .is_some_and(|p| Path::new(p).is_dir());
        if !install_ok {
            continue;
        }
        let Some(app_name) = g
            .get("app_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let title = g
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or(app_name)
            .to_string();
        // Only emit http(s) art (sideloaded titles can carry local file:// paths the client can't fetch).
        let http = |k: &str| {
            g.get(k)
                .and_then(|v| v.as_str())
                .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
                .map(String::from)
        };
        let art = Artwork {
            portrait: http("art_square"),
            header: http("art_cover"),
            hero: http("art_background").or_else(|| http("art_cover")),
            logo: http("art_logo"),
        };
        games.push(GameEntry {
            id: format!("heroic:{runner}:{app_name}"),
            store: "heroic".into(),
            title,
            art,
            launch: Some(LaunchSpec {
                kind: "heroic".into(),
                value: format!("{runner}:{app_name}"),
            }),
        });
    }
    Ok(games)
}

/// Map a `heroic` LaunchSpec value (`<runner>:<appName>`) to the Heroic launch command, run nested in
/// gamescope. The host owns this mapping; the client only ever sends the id. CAVEAT: Heroic is a
/// single-instance Electron app — in a fresh per-session gamescope it boots, launches the game (which
/// renders into that gamescope) and stays hidden via `--no-gui`; but if a Heroic GUI is ALREADY
/// running on the box, the spawned process forwards the URI and exits, which would tear the session
/// down. The validated path is the fresh-session case; needs live confirmation on a box with Heroic.
#[cfg(target_os = "linux")]
fn heroic_command(value: &str) -> Option<String> {
    let (runner, app) = value.split_once(':')?;
    if !matches!(runner, "legendary" | "gog" | "nile") {
        return None;
    }
    // appName charset (Epic alnum, GOG digits, Amazon alnum) — keep the URI a single safe token.
    if app.is_empty()
        || !app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return None;
    }
    let prefix = heroic_launch_prefix()?;
    // No quotes: gamescope spawns the app by `split_whitespace()`, and the URI has no spaces (appName
    // is validated above) so it stays a single argv token; `&` is fine (exec'd, not shell-parsed).
    Some(format!(
        "{prefix} --no-gui heroic://launch?appName={app}&runner={runner}"
    ))
}

/// How to invoke Heroic: the native `heroic` binary if on `PATH`, else the Flatpak app if its data
/// root is present. `None` ⇒ Heroic not found, so no launch command.
#[cfg(target_os = "linux")]
fn heroic_launch_prefix() -> Option<String> {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join("heroic").is_file()));
    if on_path {
        return Some("heroic".into());
    }
    let flatpak = std::env::var_os("HOME")
        .map(PathBuf::from)
        .is_some_and(|h| h.join(".var/app/com.heroicgameslauncher.hgl").is_dir());
    flatpak.then(|| "flatpak run com.heroicgameslauncher.hgl".into())
}

// ---------------------------------------------------------------------------------------
// Epic Games Store (Windows) — reads the launcher's local `.item` manifests under ProgramData
// (no auth, launcher need not run). Cover art from the base64 `catcache.bin` (public Epic CDN).
// ---------------------------------------------------------------------------------------

/// Reads the Epic Games Launcher's local install manifests. Windows-only. Best-effort: empty when
/// the launcher (or its manifest dir) isn't present.
#[cfg(windows)]
pub struct EpicProvider;

#[cfg(windows)]
impl LibraryProvider for EpicProvider {
    fn store(&self) -> &'static str {
        "epic"
    }

    fn list(&self) -> Vec<GameEntry> {
        let data = epic_data_dir();
        let Ok(rd) = std::fs::read_dir(data.join("Manifests")) else {
            return Vec::new();
        };
        // Parse the (best-effort) artwork cache ONCE: catalogItemId -> Artwork.
        let art = epic_art_index(&data.join("Catalog").join("catcache.bin"));
        let mut games = Vec::new();
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("item") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if let Some(g) = epic_entry(&v, &art) {
                games.push(g);
            }
        }
        games
    }
}

/// `%ProgramData%\Epic\EpicGamesLauncher\Data` (machine-wide, SYSTEM-readable).
#[cfg(windows)]
fn epic_data_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"))
        .join("Epic")
        .join("EpicGamesLauncher")
        .join("Data")
}

/// Map one `.item` manifest to a [`GameEntry`], or `None` if it isn't a launchable game. Uses
/// Playnite's proven EXCLUSION filter (skip `UE_*` Unreal components; skip a DLC/addon unless it is
/// `addons/launchable`) rather than a positive `games`-category match, which can drop legit titles.
#[cfg(windows)]
fn epic_entry(
    v: &serde_json::Value,
    art: &std::collections::HashMap<String, Artwork>,
) -> Option<GameEntry> {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str());
    let app_name = s("AppName")?.to_string();
    if app_name.starts_with("UE_") {
        return None; // Unreal Engine component, not a game
    }
    let cats: Vec<&str> = v
        .get("AppCategories")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    if cats.contains(&"addons") && !cats.contains(&"addons/launchable") {
        return None; // non-launchable DLC/addon
    }
    // Drop stale records whose install dir is gone.
    let install = s("InstallLocation")?;
    if !Path::new(install).is_dir() {
        return None;
    }
    let title = s("DisplayName").unwrap_or(&app_name).to_string();
    let namespace = s("CatalogNamespace").unwrap_or("");
    let catalog = s("CatalogItemId").unwrap_or("");
    // The robust launch form is the namespace:catalogItemId:appName triple; fall back to the bare
    // appName when those ids are absent (some manifests lack them) — never drop the launch entirely.
    let value = if !namespace.is_empty() && !catalog.is_empty() {
        format!("{namespace}:{catalog}:{app_name}")
    } else {
        app_name.clone()
    };
    Some(GameEntry {
        id: format!("epic:{app_name}"),
        store: "epic".into(),
        title,
        art: art.get(catalog).cloned().unwrap_or_default(),
        launch: Some(LaunchSpec {
            kind: "epic".into(),
            value,
        }),
    })
}

/// Best-effort parse of `catcache.bin` (base64-encoded JSON array of catalog items) into
/// catalogItemId → [`Artwork`] from each item's `keyImages`. Empty map on any read/decode failure
/// (the format is community-reverse-engineered + can lag a fresh install → titles just show no art).
#[cfg(windows)]
fn epic_art_index(catcache: &Path) -> std::collections::HashMap<String, Artwork> {
    use base64::Engine as _;
    let mut map = std::collections::HashMap::new();
    let Ok(raw) = std::fs::read(catcache) else {
        return map;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(raw) else {
        return map;
    };
    let Ok(items) = serde_json::from_slice::<serde_json::Value>(&decoded) else {
        return map;
    };
    let Some(arr) = items.as_array() else {
        return map;
    };
    for item in arr {
        let Some(cat) = item
            .get("id")
            .or_else(|| item.get("catalogItemId"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        let Some(images) = item.get("keyImages").and_then(|v| v.as_array()) else {
            continue;
        };
        let mut art = Artwork::default();
        for img in images {
            let (Some(ty), Some(url)) = (
                img.get("type").and_then(|v| v.as_str()),
                img.get("url").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                continue;
            }
            match ty {
                "DieselGameBoxTall" => art.portrait = Some(url.to_string()),
                "DieselGameBox" => art.hero = Some(url.to_string()),
                "DieselGameBoxLogo" => art.logo = Some(url.to_string()),
                _ => {}
            }
        }
        if art.portrait.is_some() || art.hero.is_some() || art.logo.is_some() {
            map.insert(cat.to_string(), art);
        }
    }
    map
}

/// Build the `com.epicgames.launcher://` launch URI from a stored launch value — the triple
/// `<namespace>:<catalogItemId>:<appName>` (colons URL-encoded), or a bare `<appName>` fallback.
/// Each part is charset-validated (host-derived, but belt-and-suspenders) so no shell/URI injection.
#[cfg(windows)]
fn epic_launch_uri(value: &str) -> Option<String> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    let inner = match value.split(':').collect::<Vec<_>>().as_slice() {
        [ns, cat, app] if ok(ns) && ok(cat) && ok(app) => format!("{ns}%3A{cat}%3A{app}"),
        [app] if ok(app) => (*app).to_string(),
        _ => return None,
    };
    Some(format!(
        "com.epicgames.launcher://apps/{inner}?action=launch&silent=true"
    ))
}

// ---------------------------------------------------------------------------------------
// GOG (Windows) — registry-indexed installs + each game's `goggame-<id>.info` for a direct-exe
// launch (no Galaxy needed, dodges its cold-start/anti-cheat). Art (api.gog.com) is a follow-up.
// ---------------------------------------------------------------------------------------

/// Reads the GOG.com install registry + per-game `.info` files. Windows-only. Best-effort: empty
/// when GOG isn't installed.
#[cfg(windows)]
pub struct GogProvider;

#[cfg(windows)]
impl LibraryProvider for GogProvider {
    fn store(&self) -> &'static str {
        "gog"
    }

    fn list(&self) -> Vec<GameEntry> {
        gog_games()
    }
}

#[cfg(windows)]
fn gog_games() -> Vec<GameEntry> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;
    // 32-bit GOG writes under WOW6432Node; a 64-bit process reads the explicit path directly.
    let Ok(games_key) =
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey("SOFTWARE\\WOW6432Node\\GOG.com\\Games")
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for sub in games_key.enum_keys().flatten() {
        // The subkey name IS the GOG product id.
        let Ok(k) = games_key.open_subkey(&sub) else {
            continue;
        };
        let Ok(path) = k.get_value::<String, _>("PATH") else {
            continue;
        };
        if !Path::new(&path).is_dir() {
            continue;
        }
        let title = k
            .get_value::<String, _>("GAMENAME")
            .unwrap_or_else(|_| sub.clone());
        // Resolve the primary play task (exe + args + workdir) from goggame-<id>.info; skip if absent.
        let Some((exe, args, workdir)) = gog_play_task(&path, &sub) else {
            continue;
        };
        out.push(GameEntry {
            id: format!("gog:{sub}"),
            store: "gog".into(),
            title,
            // GOG cover art is the public api.gog.com (no key) but needs an HTTP fetch + cache off the
            // hot all_games() path — deferred; title-only for now (the client renders that gracefully).
            art: Artwork::default(),
            launch: Some(LaunchSpec {
                kind: "gog".into(),
                value: format!("{exe}\t{args}\t{workdir}"),
            }),
        });
    }
    out
}

/// The primary play task from `<install>\goggame-<id>.info`: `(absolute exe, args, working dir)`.
/// Prefers `isPrimary` + `FileTask`, else the first `FileTask`. Paths are resolved against `install`.
#[cfg(windows)]
fn gog_play_task(install: &str, id: &str) -> Option<(String, String, String)> {
    let text =
        std::fs::read_to_string(Path::new(install).join(format!("goggame-{id}.info"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tasks = v.get("playTasks")?.as_array()?;
    let is_file =
        |t: &serde_json::Value| t.get("type").and_then(|s| s.as_str()) == Some("FileTask");
    let pick = tasks
        .iter()
        .find(|t| {
            t.get("isPrimary")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
                && is_file(t)
        })
        .or_else(|| tasks.iter().find(|t| is_file(t)))?;
    let rel = pick.get("path").and_then(|s| s.as_str())?;
    let exe = Path::new(install).join(rel);
    let args = pick
        .get("arguments")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let workdir = pick
        .get("workingDir")
        .and_then(|s| s.as_str())
        .map(|w| Path::new(install).join(w))
        .unwrap_or_else(|| Path::new(install).to_path_buf());
    Some((
        exe.to_string_lossy().into_owned(),
        args,
        workdir.to_string_lossy().into_owned(),
    ))
}

/// Build the spawn `(command line, working dir)` for a `gog` launch value (`exe \t args \t workdir`,
/// all host-resolved from the operator's own disk). Direct exe — no shell, no Galaxy.
#[cfg(windows)]
fn gog_spawn(value: &str) -> Option<(String, Option<PathBuf>)> {
    let mut parts = value.split('\t');
    let exe = parts.next().filter(|s| !s.is_empty())?;
    let args = parts.next().unwrap_or("");
    let workdir = parts.next().filter(|s| !s.is_empty()).map(PathBuf::from);
    let cmdline = if args.trim().is_empty() {
        format!("\"{exe}\"")
    } else {
        format!("\"{exe}\" {args}")
    };
    Some((cmdline, workdir))
}

// ---------------------------------------------------------------------------------------
// Xbox / Microsoft Store / Game Pass (Windows) — scans the flat-file `XboxGames` install dirs
// (no auth) for GDK games (each has a Content\MicrosoftGame.config). Launch via the AUMID
// (shell:AppsFolder\<PFN>!<AppId>) in the interactive session. Cover art (displaycatalog) deferred.
// ---------------------------------------------------------------------------------------

/// Reads installed Xbox / Game Pass / Store GDK games from the flat-file install dirs. Windows-only.
/// Best-effort: empty when no `XboxGames` dir exists.
#[cfg(windows)]
pub struct XboxProvider;

#[cfg(windows)]
impl LibraryProvider for XboxProvider {
    fn store(&self) -> &'static str {
        "xbox"
    }

    fn list(&self) -> Vec<GameEntry> {
        xbox_games()
    }
}

/// Scan each fixed drive's default `<drive>:\XboxGames` for GDK games — the presence of
/// `Content\MicrosoftGame.config` is the game marker (so we list games, not ordinary UWP apps). A
/// custom install folder (set via the undocumented `.GamingRoot`) isn't covered; the default folder
/// is the common case. Non-GDK pure-UWP Store games (under the ACL-locked WindowsApps) are missed too.
#[cfg(windows)]
fn xbox_games() -> Vec<GameEntry> {
    let mut games = Vec::new();
    for letter in b'C'..=b'Z' {
        let root = PathBuf::from(format!("{}:\\XboxGames", letter as char));
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.flatten() {
            let title_dir = entry.path();
            let cfg = title_dir.join("Content").join("MicrosoftGame.config");
            if !cfg.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&cfg) else {
                continue;
            };
            let folder = title_dir
                .file_name()
                .map(|f| f.to_string_lossy().into_owned());
            let Some((name, app_id, title, store_id)) = xbox_parse_config(&text, folder.as_deref())
            else {
                continue;
            };
            let Some(pfn) = xbox_pfn(&name) else {
                tracing::debug!(package = %name, "xbox: no AppRepository entry → can't resolve PFN, skipping");
                continue;
            };
            let id_key = if store_id.is_empty() {
                pfn.clone()
            } else {
                store_id
            };
            games.push(GameEntry {
                id: format!("xbox:{id_key}"),
                store: "xbox".into(),
                title,
                // displaycatalog.mp.microsoft.com cover art (no key, but unofficial + needs an HTTP
                // fetch + cache off the hot path) is deferred → title-only (rendered gracefully).
                art: Artwork::default(),
                launch: Some(LaunchSpec {
                    kind: "aumid".into(),
                    value: format!("{pfn}!{app_id}"),
                }),
            });
        }
    }
    games.sort_by(|a, b| a.id.cmp(&b.id));
    games.dedup_by(|a, b| a.id == b.id); // same game on two drives → one entry
    games
}

/// Parse the fields we need from a `MicrosoftGame.config`: `(Identity Name, AppId, title, StoreId)`.
/// AppId is the `<Executable>`'s `Id` (the AUMID app id, typically "Game"). The title prefers
/// `ShellVisuals@DefaultDisplayName`, but that can be an unresolved `ms-resource:` ref → fall back to
/// the install folder name, then the package name.
#[cfg(windows)]
fn xbox_parse_config(text: &str, folder: Option<&str>) -> Option<(String, String, String, String)> {
    let doc = roxmltree::Document::parse(text).ok()?;
    let root = doc.root_element();
    let name = root
        .children()
        .find(|n| n.has_tag_name("Identity"))?
        .attribute("Name")?
        .to_string();
    let app_id = root
        .children()
        .find(|n| n.has_tag_name("ExecutableList"))
        .and_then(|el| {
            el.children()
                .filter(|n| n.has_tag_name("Executable"))
                .find_map(|e| e.attribute("Id"))
        })?
        .to_string();
    let ddn = root
        .children()
        .find(|n| n.has_tag_name("ShellVisuals"))
        .and_then(|sv| sv.attribute("DefaultDisplayName"))
        .filter(|s| !s.is_empty() && !s.starts_with("ms-resource"));
    let title = ddn
        .map(String::from)
        .or_else(|| folder.map(String::from))
        .unwrap_or_else(|| name.clone());
    let store_id = root
        .children()
        .find(|n| n.has_tag_name("StoreId"))
        .and_then(|n| n.text())
        .unwrap_or("")
        .to_string();
    Some((name, app_id, title, store_id))
}

/// Resolve a package's PackageFamilyName by finding its
/// `AppRepository\Packages\<PackageFullName>` dir (machine-wide, SYSTEM-readable) and reducing the
/// full name to `Name_PublisherHash`. This READS the authoritative PFN — never compute the hash.
#[cfg(windows)]
fn xbox_pfn(identity: &str) -> Option<String> {
    let pkgs = PathBuf::from(std::env::var_os("ProgramData")?)
        .join("Microsoft")
        .join("Windows")
        .join("AppRepository")
        .join("Packages");
    let prefix = format!("{identity}_");
    for e in std::fs::read_dir(&pkgs).ok()?.flatten() {
        let dn = e.file_name().to_string_lossy().into_owned();
        if dn.starts_with(&prefix) {
            if let Some(pfn) = pfn_from_full(&dn, identity) {
                return Some(pfn);
            }
        }
    }
    None
}

/// PackageFamilyName from a PackageFullName dir name
/// (`Name_Version_Arch_ResourceId_PublisherHash`) → `Name_PublisherHash`. The hash is the last
/// `_`-segment; `Name` is the caller's identity.
#[cfg(windows)]
fn pfn_from_full(dir_name: &str, identity: &str) -> Option<String> {
    let hash = dir_name.rsplit('_').next()?;
    (!hash.is_empty() && hash != dir_name).then(|| format!("{identity}_{hash}"))
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

/// A digits-only Steam appid: the sole client-influenced part of a Steam launch, validated before it
/// is interpolated into any command / URI (so a client-sent id can never carry shell or URI syntax).
/// Cross-platform — used by the Linux shell mapping ([`command_for`]) and the Windows spawn mapping
/// ([`windows_launch_for`]).
fn valid_steam_appid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// Resolve a store-qualified library id (as sent by a client in `Hello::launch`) to the shell
/// command the host should run for it — looked up in the host's OWN library so a client can only
/// pick an existing title, never inject a command. `None` = unknown id, no launch recipe, or a
/// malformed Steam appid.
///
/// **Linux only**: the resolved command is run nested inside the per-session gamescope. On Windows
/// there is no gamescope to nest into; the host launches a title into the interactive user session
/// via [`launch_title`] instead.
///
/// - `steam_appid` → `steam steam://rungameid/<appid>` (appid validated as digits).
/// - `command` → the stored command verbatim. This string comes from the host's own custom store
///   (added by the host operator via the admin UI), never from the client, so it is trusted.
#[cfg(not(windows))]
pub fn launch_command(id: &str) -> Option<String> {
    let spec = all_games().into_iter().find(|g| g.id == id)?.launch?;
    command_for(&spec)
}

/// Map a resolved [`LaunchSpec`] to its shell command (pure — the unit-testable core of
/// [`launch_command`], split out so the appid-validation can be tested without a Steam install).
#[cfg(not(windows))]
fn command_for(spec: &LaunchSpec) -> Option<String> {
    match spec.kind.as_str() {
        "steam_appid" => valid_steam_appid(&spec.value)
            .then(|| format!("steam steam://rungameid/{}", spec.value)),
        // Lutris: a digits-only pga.db game id (same guard as steam_appid) → its run URI.
        #[cfg(target_os = "linux")]
        "lutris_id" => (!spec.value.is_empty() && spec.value.bytes().all(|b| b.is_ascii_digit()))
            .then(|| format!("lutris lutris:rungameid/{}", spec.value)),
        // Heroic: `<runner>:<appName>` → the validated heroic://launch command (see heroic_command).
        #[cfg(target_os = "linux")]
        "heroic" => heroic_command(&spec.value),
        // Trusted: the command comes from the host's own custom store, never the client.
        "command" => (!spec.value.trim().is_empty()).then(|| spec.value.clone()),
        _ => None,
    }
}

/// Windows: launch a store-qualified library id into the **interactive user session** — the Windows
/// analogue of the Linux gamescope-nested [`launch_command`]. The id is resolved against the host's
/// OWN library (the client never sends a command), mapped to a concrete process by
/// [`windows_launch_for`], and spawned via [`crate::interactive::spawn_in_active_session`].
///
/// Wired into the data plane *after* capture is live, so the title renders onto the already-captured
/// desktop and grabs foreground.
#[cfg(windows)]
pub fn launch_title(id: &str) -> Result<()> {
    let spec = all_games()
        .into_iter()
        .find(|g| g.id == id)
        .and_then(|g| g.launch)
        .ok_or_else(|| anyhow::anyhow!("no launchable library entry '{id}'"))?;
    let (cmdline, workdir) = windows_launch_for(&spec).ok_or_else(|| {
        anyhow::anyhow!(
            "library entry '{id}' has no Windows launch recipe (kind '{}')",
            spec.kind
        )
    })?;
    let pid = crate::interactive::spawn_in_active_session(&cmdline, workdir.as_deref())
        .with_context(|| format!("launch '{id}' in the interactive session"))?;
    tracing::info!(launch_id = id, %cmdline, pid, "launched library title in the interactive session");
    Ok(())
}

/// Windows: map a resolved [`LaunchSpec`] to a `(command line, working dir)` to spawn into the
/// interactive session. Pure + unit-testable. `None` = no Windows recipe for this kind.
///
/// CreateProcessAsUserW does NO shell or protocol resolution, so the URI/flags are handed to a
/// concrete EXE as plain arguments — a (host-derived) URI string can never reach a command interpreter.
#[cfg(windows)]
fn windows_launch_for(spec: &LaunchSpec) -> Option<(String, Option<std::path::PathBuf>)> {
    match spec.kind.as_str() {
        "steam_appid" => {
            if !valid_steam_appid(&spec.value) {
                return None;
            }
            let uri = format!("steam://rungameid/{}", spec.value);
            // Prefer launching Steam.exe with the URI as an argument; fall back to explorer.exe, which
            // resolves the steam:// handler from the user hive. (The appid is digits-validated, so the
            // only variable part of the line is a number either way.)
            let cmdline = match steam_exe() {
                Some(exe) => format!("\"{}\" \"{uri}\"", exe.display()),
                None => format!("explorer.exe \"{uri}\""),
            };
            Some((cmdline, None))
        }
        // Epic: open the (host-built, validated) com.epicgames.launcher:// URI via explorer.exe — a
        // concrete EXE that resolves the registered protocol handler as the user; the URI is a single
        // argv element (no shell, no cmd /c). Same pattern as the steam explorer fallback.
        "epic" => epic_launch_uri(&spec.value).map(|uri| (format!("explorer.exe \"{uri}\""), None)),
        // GOG: spawn the resolved game exe directly (host-derived from goggame-<id>.info), no Galaxy.
        "gog" => gog_spawn(&spec.value),
        // Xbox/Game Pass: activate the UWP/GDK package by its AUMID (<PFN>!<AppId>) via explorer's
        // shell:AppsFolder — which runs in the interactive user session (UWP activation fails as
        // SYSTEM/session-0; spawn_in_active_session uses the user token). Guard the charset (the value
        // is host-derived from MicrosoftGame.config + AppRepository, but belt-and-suspenders).
        "aumid" => {
            let valid = spec.value.split_once('!').is_some_and(|(pfn, app)| {
                let part = |s: &str| {
                    !s.is_empty()
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
                };
                part(pfn) && part(app)
            });
            valid.then(|| {
                (
                    format!("explorer.exe \"shell:AppsFolder\\{}\"", spec.value),
                    None,
                )
            })
        }
        // Operator-typed custom command (host-owned, never client-set): run it through the shell in the
        // interactive session. `cmd.exe /c` is acceptable here precisely because the value is operator
        // input — the same trust as the operator typing it — not a client-influenced string.
        "command" => {
            let v = spec.value.trim();
            (!v.is_empty()).then(|| (format!("cmd.exe /c {v}"), None))
        }
        _ => None,
    }
}

/// Windows: the default Steam install's `steam.exe`, if present. A non-default Steam install dir
/// (registry `Valve\Steam\InstallPath`) isn't covered — the explorer.exe protocol fallback handles
/// that case. Mirrors [`steam_roots`]' "default Program Files dirs" approach.
#[cfg(windows)]
fn steam_exe() -> Option<std::path::PathBuf> {
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            let p = std::path::PathBuf::from(pf).join("Steam").join("steam.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// The full library: every store's titles merged + the custom entries, sorted by title.
pub fn all_games() -> Vec<GameEntry> {
    let mut games = SteamProvider.list();
    // The Lutris + Heroic providers are Linux-only (their launchers are); on other hosts the library
    // is Steam + custom. Each provider is best-effort (empty when its store isn't present).
    #[cfg(target_os = "linux")]
    {
        games.extend(LutrisProvider.list());
        games.extend(HeroicProvider.list());
    }
    // Windows store providers (their launchers are Windows-only): Epic + GOG + Xbox/Game Pass.
    #[cfg(windows)]
    {
        games.extend(EpicProvider.list());
        games.extend(GogProvider.list());
        games.extend(XboxProvider.list());
    }
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

    #[cfg(not(windows))]
    #[test]
    fn launch_command_resolves_and_guards() {
        let steam = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570".into(),
        };
        assert_eq!(
            command_for(&steam).as_deref(),
            Some("steam steam://rungameid/570")
        );
        // A non-numeric "appid" (e.g. a client trying to inject) is rejected, never interpolated.
        let evil = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570; rm -rf ~".into(),
        };
        assert_eq!(command_for(&evil), None);
        // Custom commands (from the host's own store) pass through verbatim.
        let custom = LaunchSpec {
            kind: "command".into(),
            value: "dolphin-emu --batch".into(),
        };
        assert_eq!(command_for(&custom).as_deref(), Some("dolphin-emu --batch"));
        // Empty / unknown kinds → no command.
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "command".into(),
                value: "  ".into()
            }),
            None
        );
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "wat".into(),
                value: "x".into()
            }),
            None
        );
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

    #[cfg(target_os = "linux")]
    #[test]
    fn lutris_games_reads_installed_only() {
        use rusqlite::Connection;
        let dir = std::env::temp_dir().join(format!("pf-lutris-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("pga.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch(
                "CREATE TABLE games (id INTEGER PRIMARY KEY, slug TEXT, name TEXT, installed INTEGER);
                 INSERT INTO games (id,slug,name,installed) VALUES (42,'elden-ring','ELDEN RING',1);
                 INSERT INTO games (id,slug,name,installed) VALUES (7,'owned','Owned Only',0);
                 INSERT INTO games (id,slug,name,installed) VALUES (9,'noname',NULL,1);",
            )
            .unwrap();
        }
        let games = lutris_games(&db).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        // Only the installed, named row; the uninstalled + NULL-name rows are filtered out.
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].id, "lutris:42");
        assert_eq!(games[0].store, "lutris");
        assert_eq!(games[0].title, "ELDEN RING");
        let l = games[0].launch.as_ref().unwrap();
        assert_eq!((l.kind.as_str(), l.value.as_str()), ("lutris_id", "42"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn heroic_games_parses_installed_with_cdn_art() {
        let dir = std::env::temp_dir().join(format!("pf-heroic-test-{}", std::process::id()));
        let install = dir.join("game-install");
        std::fs::create_dir_all(&install).unwrap();
        let path = dir.join("legendary_library.json");
        let json = format!(
            r#"{{"library":[
                {{"app_name":"Quail","title":"Quail","is_installed":true,
                  "install":{{"install_path":"{inst}"}},
                  "art_square":"https://cdn/quail_tall.jpg","art_cover":"https://cdn/quail_wide.jpg",
                  "art_logo":"file:///local/logo.png"}},
                {{"app_name":"Owned","title":"Owned Only","is_installed":false,
                  "install":{{"install_path":"{inst}"}}}}
            ]}}"#,
            inst = install.display()
        );
        std::fs::write(&path, json).unwrap();
        let games = heroic_games(&path, "legendary", "library").unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(games.len(), 1); // the uninstalled title is filtered out
        assert_eq!(games[0].id, "heroic:legendary:Quail");
        assert_eq!(games[0].title, "Quail");
        assert_eq!(
            games[0].art.portrait.as_deref(),
            Some("https://cdn/quail_tall.jpg")
        );
        assert_eq!(
            games[0].art.header.as_deref(),
            Some("https://cdn/quail_wide.jpg")
        );
        assert!(games[0].art.logo.is_none()); // file:// art is dropped (client can't fetch it)
        let l = games[0].launch.as_ref().unwrap();
        assert_eq!(
            (l.kind.as_str(), l.value.as_str()),
            ("heroic", "legendary:Quail")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn command_for_lutris_and_heroic_guards() {
        // Lutris: digits → its run URI; a non-numeric id (injection attempt) is rejected.
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "lutris_id".into(),
                value: "42".into()
            })
            .as_deref(),
            Some("lutris lutris:rungameid/42")
        );
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "lutris_id".into(),
                value: "42; rm -rf ~".into()
            }),
            None
        );
        // Heroic guards (independent of whether Heroic is installed): bad runner / appName → None.
        assert_eq!(heroic_command("badrunner:Quail"), None);
        assert_eq!(heroic_command("legendary:bad name"), None);
        assert_eq!(heroic_command("nile:"), None);
        // When Heroic IS resolvable (a dev box), a valid id yields the launch URI; on CI (no Heroic)
        // it's None — assert the URI shape only when a launcher prefix exists.
        if let Some(cmd) = heroic_command("legendary:Quail-1.2_x") {
            assert!(cmd.contains("heroic://launch?appName=Quail-1.2_x&runner=legendary"));
            assert!(cmd.contains("--no-gui"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_launch_for_maps_and_guards() {
        // Steam: a digits-only appid → a steam:// URI line (via Steam.exe or explorer.exe, depending
        // on the box) with no working dir.
        let steam = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570".into(),
        };
        let (line, wd) = windows_launch_for(&steam).expect("steam recipe");
        assert!(line.contains("steam://rungameid/570"), "line was {line:?}");
        assert!(wd.is_none());
        // A non-numeric "appid" (a client trying to inject) is rejected, never interpolated.
        let evil = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570\" & calc".into(),
        };
        assert!(windows_launch_for(&evil).is_none());
        // Operator command → cmd /c passthrough (trusted host input).
        let cmd = LaunchSpec {
            kind: "command".into(),
            value: "notepad.exe".into(),
        };
        assert_eq!(
            windows_launch_for(&cmd).unwrap().0,
            "cmd.exe /c notepad.exe"
        );
        // Xbox AUMID → explorer shell:AppsFolder activation; a value without '!' is rejected.
        let aumid = LaunchSpec {
            kind: "aumid".into(),
            value: "Microsoft.X_8wekyb3d8bbwe!Game".into(),
        };
        assert_eq!(
            windows_launch_for(&aumid).unwrap().0,
            "explorer.exe \"shell:AppsFolder\\Microsoft.X_8wekyb3d8bbwe!Game\""
        );
        assert!(windows_launch_for(&LaunchSpec {
            kind: "aumid".into(),
            value: "no-bang".into()
        })
        .is_none());
        // Empty / unknown kinds → no recipe.
        assert!(windows_launch_for(&LaunchSpec {
            kind: "command".into(),
            value: "  ".into()
        })
        .is_none());
        assert!(windows_launch_for(&LaunchSpec {
            kind: "wat".into(),
            value: "x".into()
        })
        .is_none());
    }

    #[cfg(windows)]
    #[test]
    fn epic_filters_and_builds_launch() {
        let dir = std::env::temp_dir().join(format!("pf-epic-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let inst = dir.to_string_lossy().into_owned();
        let empty = std::collections::HashMap::new();
        // Normal game with the full triple → kept, triple launch value.
        let game = serde_json::json!({
            "AppName": "Fortnite", "DisplayName": "Fortnite", "CatalogNamespace": "fn",
            "CatalogItemId": "abc123", "InstallLocation": inst.clone(),
            "AppCategories": ["public", "games", "applications"]
        });
        let e = epic_entry(&game, &empty).expect("game kept");
        assert_eq!(e.id, "epic:Fortnite");
        assert_eq!(e.launch.as_ref().unwrap().value, "fn:abc123:Fortnite");
        // UE component, non-launchable addon, and a missing install dir are all skipped.
        let ue = serde_json::json!({"AppName":"UE_5.3","InstallLocation":inst.clone(),"AppCategories":["engines"]});
        assert!(epic_entry(&ue, &empty).is_none());
        let dlc =
            serde_json::json!({"AppName":"DLC","InstallLocation":inst,"AppCategories":["addons"]});
        assert!(epic_entry(&dlc, &empty).is_none());
        let gone = serde_json::json!({"AppName":"Gone","InstallLocation":"C:\\nope-xyz","AppCategories":["games"]});
        assert!(epic_entry(&gone, &empty).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(windows)]
    #[test]
    fn epic_launch_uri_triple_bare_and_guard() {
        assert_eq!(
            epic_launch_uri("fn:abc:Fortnite").as_deref(),
            Some("com.epicgames.launcher://apps/fn%3Aabc%3AFortnite?action=launch&silent=true")
        );
        assert_eq!(
            epic_launch_uri("Fortnite").as_deref(),
            Some("com.epicgames.launcher://apps/Fortnite?action=launch&silent=true")
        );
        assert!(epic_launch_uri("bad part:x:y").is_none()); // a space → rejected
        assert!(epic_launch_uri("").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn gog_spawn_parses_and_guards() {
        let (cmd, wd) = gog_spawn("C:\\Games\\W3\\witcher3.exe\t--skip\tC:\\Games\\W3").unwrap();
        assert_eq!(cmd, "\"C:\\Games\\W3\\witcher3.exe\" --skip");
        assert_eq!(wd, Some(std::path::PathBuf::from("C:\\Games\\W3")));
        let (cmd2, wd2) = gog_spawn("C:\\g.exe").unwrap();
        assert_eq!(cmd2, "\"C:\\g.exe\"");
        assert!(wd2.is_none());
        assert!(gog_spawn("").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn gog_play_task_picks_primary_filetask() {
        let dir = std::env::temp_dir().join(format!("pf-gog-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = "1207658924";
        std::fs::write(
            dir.join(format!("goggame-{id}.info")),
            r#"{"playTasks":[
                {"isPrimary":false,"type":"FileTask","path":"other.exe"},
                {"isPrimary":true,"type":"FileTask","path":"bin\\game.exe","arguments":"-w","workingDir":"bin"}
            ]}"#,
        )
        .unwrap();
        let (exe, args, wd) = gog_play_task(&dir.to_string_lossy(), id).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(exe.ends_with("bin\\game.exe"), "exe={exe}");
        assert_eq!(args, "-w");
        assert!(wd.ends_with("bin"), "wd={wd}");
    }

    #[cfg(windows)]
    #[test]
    fn xbox_parse_config_and_pfn() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Game configVersion="1">
  <Identity Name="Microsoft.624F8B84B80" Publisher="CN=Microsoft" Version="1.0.0.0" />
  <ExecutableList>
    <Executable Name="gamelaunchhelper.exe" Id="Game" />
  </ExecutableList>
  <StoreId>9NBLGGH4R315</StoreId>
  <ShellVisuals DefaultDisplayName="Halo Infinite" Square150x150Logo="x.png" />
</Game>"#;
        let (name, app_id, title, store_id) = xbox_parse_config(xml, Some("HaloInfinite")).unwrap();
        assert_eq!(name, "Microsoft.624F8B84B80");
        assert_eq!(app_id, "Game");
        assert_eq!(title, "Halo Infinite");
        assert_eq!(store_id, "9NBLGGH4R315");
        // An ms-resource DefaultDisplayName is unresolvable → fall back to the install folder name.
        let xml2 = r#"<Game><Identity Name="Pkg.Name"/>
            <ExecutableList><Executable Id="App"/></ExecutableList>
            <ShellVisuals DefaultDisplayName="ms-resource:DisplayName"/></Game>"#;
        let (_, app2, title2, sid2) = xbox_parse_config(xml2, Some("MyGameFolder")).unwrap();
        assert_eq!(app2, "App");
        assert_eq!(title2, "MyGameFolder");
        assert_eq!(sid2, "");
        // PackageFamilyName reduced from a PackageFullName dir name (the hash is the last segment).
        assert_eq!(
            pfn_from_full(
                "Microsoft.624F8B84B80_1.0.0.0_x64__8wekyb3d8bbwe",
                "Microsoft.624F8B84B80"
            )
            .as_deref(),
            Some("Microsoft.624F8B84B80_8wekyb3d8bbwe")
        );
        assert!(pfn_from_full("NoUnderscore", "NoUnderscore").is_none());
    }
}
