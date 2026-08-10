//! Heroic (Epic/GOG) store provider: installed games from Heroic's JSON stores + CDN art. Split out of the `library` facade (plan §W5).

use super::*;

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
        // The install dir doubles as this title's detect signal (Heroic hands off to
        // legendary/gogdl/nile, so the host never sees the game's own process any other way).
        let install_path = g
            .get("install")
            .and_then(|i| i.get("install_path"))
            .and_then(|p| p.as_str())
            .filter(|p| Path::new(p).is_dir());
        let Some(install_path) = install_path else {
            continue;
        };
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
            provider: None,
            role: GameRole::Game,
            icon: None,
            meta: GameMeta::pc(),
            id: format!("heroic:{runner}:{app_name}"),
            store: "heroic".into(),
            title,
            art,
            launch: Some(LaunchSpec {
                kind: "heroic".into(),
                value: format!("{runner}:{app_name}"),
            }),
            // The install dir is the reliable signal. `HEROIC_APP_NAME` is also stamped on the game's
            // env by Heroic's launch path; it is carried as a second, cheap signal (a union — if a
            // Heroic version doesn't set it, the install dir still matches).
            detect: DetectSpec::dir(install_path)
                .with_env("HEROIC_APP_NAME", Some(app_name.to_string())),
        });
    }
    Ok(games)
}

// The `heroic` launch mapping (`heroic_command` + its launcher-prefix probe) lives in `launch.rs`
// (WP1.1) — this module enumerates, it does not launch.

#[cfg(test)]
mod tests {
    use super::*;

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
}
