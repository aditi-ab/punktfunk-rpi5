//! Epic Games Store provider: installed manifests + the catalog-cache art index + launch URIs. Split out of the `library` facade (plan §W5).

use super::*;

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
            // `.item` manifests are small JSON; cap the read so a planted giant can't OOM the host.
            let Some(bytes) = read_capped(&p, 1024 * 1024) else {
                continue;
            };
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
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
    // Detect signals: the manifest's own `LaunchExecutable` (relative to the install dir) is exact
    // when present; the install dir covers the rest (Epic hands off to its launcher, so the host
    // never owns the game's process).
    let detect = match s("LaunchExecutable")
        .map(|rel| Path::new(install).join(rel))
        .filter(|p| p.is_file())
    {
        Some(exe) => DetectSpec::exe(exe).with_dir(install),
        None => DetectSpec::dir(install),
    };
    Some(GameEntry {
        provider: None,
        meta: GameMeta::pc(),
        id: format!("epic:{app_name}"),
        store: "epic".into(),
        title,
        art: art.get(catalog).cloned().unwrap_or_default(),
        launch: Some(LaunchSpec {
            kind: "epic".into(),
            value,
        }),
        detect,
    })
}

/// Read a launcher cache/manifest with a hard size cap, so a local unprivileged user can't plant a
/// multi-GB file under the launcher's (Users-writable) data dir that OOMs the privileged host when
/// it's loaded — then base64/JSON-decoded into further copies — during library enumeration
/// (security-review 2026-06-28 S4). Returns `None` if missing, empty, or over `max`. Mirrors the
/// Linux lutris-art reader's 1 MiB cap.
#[cfg(windows)]
fn read_capped(path: &Path, max: u64) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() == 0 || meta.len() > max {
        if meta.len() > max {
            tracing::warn!(path = %path.display(), len = meta.len(), max, "launcher cache exceeds size cap — skipping");
        }
        return None;
    }
    std::fs::read(path).ok()
}

/// Best-effort parse of `catcache.bin` (base64-encoded JSON array of catalog items) into
/// catalogItemId → [`Artwork`] from each item's `keyImages`. Empty map on any read/decode failure
/// (the format is community-reverse-engineered + can lag a fresh install → titles just show no art).
#[cfg(windows)]
fn epic_art_index(catcache: &Path) -> std::collections::HashMap<String, Artwork> {
    use base64::Engine as _;
    let mut map = std::collections::HashMap::new();
    // 32 MiB cap: comfortably fits a real catalog cache, blocks a planted giant (S4).
    let Some(raw) = read_capped(catcache, 32 * 1024 * 1024) else {
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

// The `epic` launch mapping (`epic_launch_uri`) lives in `launch.rs` (WP1.1) — this module
// enumerates, it does not launch.

#[cfg(test)]
mod tests {
    use super::*;

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
}
