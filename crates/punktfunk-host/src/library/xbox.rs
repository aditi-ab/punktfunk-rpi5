//! Xbox / Microsoft Store (UWP) provider: installed packages, PFN resolution, and store art. Split out of the `library` facade (plan §W5).

use super::art::cached_art;
use super::*;

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
            // Cap the read like the other untrusted on-disk manifests (Epic `read_capped`, Lutris
            // art) — a planted multi-GB MicrosoftGame.config under `<drive>:\XboxGames\…\Content\`
            // must not OOM the privileged host during enumeration (security-review 2026-07-17). A
            // real GDK manifest is a few KB.
            match cfg.metadata() {
                Ok(m) if m.len() <= 1024 * 1024 => {}
                _ => continue,
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
            let id = format!("xbox:{id_key}");
            // Art (unofficial displaycatalog, keyed by StoreId) is resolved off the hot path by the
            // background warmer; read whatever it has cached (title-only until warmed / if no StoreId).
            let art = cached_art(&id).unwrap_or_default();
            games.push(GameEntry {
                provider: None,
                id,
                store: "xbox".into(),
                title,
                art,
                launch: Some(LaunchSpec {
                    kind: "aumid".into(),
                    value: format!("{pfn}!{app_id}"),
                }),
                // AUMID activation goes through the shell, so the host never owns the process: the
                // title's `Content` dir (which holds the game's binaries) is the detect signal.
                detect: DetectSpec::dir(title_dir.join("Content")),
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

#[cfg(test)]
mod tests {
    use super::*;

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
