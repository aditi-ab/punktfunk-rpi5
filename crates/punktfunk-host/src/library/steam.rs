//! Steam store provider: installed-title scan (local `libraryfolders.vdf` + app manifests, no
//! API key) and Steam-CDN / local-`librarycache` artwork. Split out of the `library` facade (plan §W5).

use super::art::fetch_image;
use super::*;

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
                provider: None,
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

/// The Steam CDN poster/hero/logo/header for an appid — relative proxy paths the *client* resolves
/// against the host it just talked to (so they work the same whichever interface/port the client
/// reached the host on), backed by [`steam_art_bytes`] on the way out. Not every appid has a
/// 600×900 capsule, but `header.jpg` is effectively universal — the client falls back to it.
fn steam_art(appid: u32) -> Artwork {
    let url = |kind: &str| Some(format!("/api/v1/library/art/steam:{appid}/{kind}"));
    Artwork {
        portrait: url("portrait"),
        hero: url("hero"),
        logo: url("logo"),
        header: url("header"),
    }
}

/// Resolve one Steam cover-art kind to bytes: the host's own local Steam cache first (exact — it's
/// literally what the user's Steam client already shows for this title), the legacy flat CDN URL
/// as a fallback. `None` when neither has it (the client then falls through to its next art
/// candidate). Blocking (disk + network) — call off the async runtime.
pub fn steam_art_bytes(appid: u32, kind: ArtKind) -> Option<(Vec<u8>, String)> {
    steam_local_art_bytes(appid, kind).or_else(|| {
        let url = format!(
            "https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}/{}",
            kind.cdn_filename()
        );
        fetch_image(&url)
    })
}

/// Cap on a local librarycache file we'll read into memory — generous for a Steam-quality JPEG/PNG
/// (these run well under 2 MiB in practice) while bounding a pathological file.
const LOCAL_ART_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// `appcache/librarycache/<appid>/<hash>/<filename>` across every Steam root, for whichever
/// `<hash>` subdirectory actually has this kind's file (Steam reuses one hash dir per asset
/// version, so there's normally exactly one candidate per kind).
fn steam_local_art_bytes(appid: u32, kind: ArtKind) -> Option<(Vec<u8>, String)> {
    steam_roots()
        .into_iter()
        .find_map(|root| find_local_art_file(&root, appid, kind))
        .and_then(|path| {
            let bytes = std::fs::read(&path).ok()?;
            let ctype = if path.extension().is_some_and(|e| e == "png") {
                "image/png"
            } else {
                "image/jpeg"
            };
            Some((bytes, ctype.to_string()))
        })
}

/// Find this kind's cached file under one Steam root's `appcache/librarycache/<appid>/<hash>/`,
/// trying each hash subdirectory (normally just one) and each candidate filename in priority
/// order. Pure path lookup — no env/HOME dependency — so it's unit-testable against a plain
/// directory fixture.
fn find_local_art_file(root: &Path, appid: u32, kind: ArtKind) -> Option<PathBuf> {
    let cache_dir = root
        .join("appcache")
        .join("librarycache")
        .join(appid.to_string());
    let hash_dirs = std::fs::read_dir(&cache_dir).ok()?;
    for hash_dir in hash_dirs.flatten() {
        for name in kind.local_filenames() {
            let path = hash_dir.path().join(name);
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.len() > 0 && meta.len() <= LOCAL_ART_MAX_BYTES {
                return Some(path);
            }
        }
    }
    None
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
    fn steam_art_points_at_the_host_art_proxy() {
        let art = steam_art(570);
        assert_eq!(
            art.portrait.as_deref(),
            Some("/api/v1/library/art/steam:570/portrait")
        );
        assert_eq!(
            art.header.as_deref(),
            Some("/api/v1/library/art/steam:570/header")
        );
    }

    #[test]
    fn find_local_art_file_matches_the_hashed_librarycache_layout() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir
            .path()
            .join("appcache/librarycache/3527290/480bd879ac737921bfa2529a6fea15961267ad21");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("library_600x900.jpg"), b"not really a jpeg").unwrap();

        let found = find_local_art_file(dir.path(), 3527290, ArtKind::Portrait).unwrap();
        assert_eq!(found, cache.join("library_600x900.jpg"));
        // A kind with no cached file, and an appid with no cache dir at all, both miss cleanly.
        assert_eq!(
            find_local_art_file(dir.path(), 3527290, ArtKind::Hero),
            None
        );
        assert_eq!(
            find_local_art_file(dir.path(), 570, ArtKind::Portrait),
            None
        );
    }

    #[test]
    fn find_local_art_file_prefers_the_2x_portrait() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("appcache/librarycache/570/somehash");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("library_600x900.jpg"), b"1x").unwrap();
        std::fs::write(cache.join("library_600x900_2x.jpg"), b"2x").unwrap();

        let found = find_local_art_file(dir.path(), 570, ArtKind::Portrait).unwrap();
        assert_eq!(found, cache.join("library_600x900_2x.jpg"));
    }
}
