//! Steam store provider: installed-title scan (local `libraryfolders.vdf` + app manifests, no
//! API key) and Steam-CDN / local-`librarycache` artwork. Split out of the `library` facade (plan §W5).

use super::art::fetch_image;
use super::*;

/// Reads the **local** Steam install — no Steam Web API key, no network. Installed titles come
/// from `steamapps/appmanifest_<appid>.acf`; extra library folders from
/// `steamapps/libraryfolders.vdf`; the user's own non-Steam shortcuts ("Add a Non-Steam Game to My
/// Library") from each account's binary `userdata/<id>/config/shortcuts.vdf`; artwork from the
/// public Steam CDN by appid, or the user's per-account `grid/` overrides (all a shortcut ever has).
pub struct SteamProvider;

impl LibraryProvider for SteamProvider {
    fn store(&self) -> &'static str {
        "steam"
    }

    fn list(&self) -> Vec<GameEntry> {
        let mut by_appid: std::collections::BTreeMap<u32, Installed> = Default::default();
        for steamapps in steam_library_dirs() {
            for app in scan_manifests(&steamapps) {
                // First library wins; dedups appids present in several libraries.
                by_appid.entry(app.appid).or_insert(app);
            }
        }
        let mut games: Vec<GameEntry> = by_appid
            .into_values()
            .filter(|app| !is_steam_tool(app.appid, &app.name))
            .map(|app| GameEntry {
                provider: None,
                role: GameRole::Game,
                meta: GameMeta::pc(),
                id: format!("steam:{}", app.appid),
                store: "steam".into(),
                art: steam_art(app.appid),
                launch: Some(LaunchSpec {
                    kind: "steam_appid".into(),
                    value: app.appid.to_string(),
                }),
                // The appid alone is authoritative on Linux (Steam's launch reaper); the install dir
                // is what the Windows matcher — which has no reaper to watch — keys off instead.
                detect: match app.install_dir {
                    Some(dir) => DetectSpec::steam(app.appid).with_dir(dir),
                    None => DetectSpec::steam(app.appid),
                },
                title: app.name,
            })
            .collect();
        // Non-Steam shortcuts have no `appmanifest` — [`scan_manifests`] can't see them, so the
        // user's own custom entries are gathered separately from `shortcuts.vdf`.
        games.extend(steam_shortcuts());
        games
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
/// literally what the user's Steam client already shows for this title), then the user's per-account
/// `grid/` overrides (the *only* art a non-Steam shortcut ever has), then the legacy flat CDN URL.
/// `None` when none has it (the client then falls through to its next art candidate). Blocking
/// (disk + network) — call off the async runtime.
pub fn steam_art_bytes(appid: u32, kind: ArtKind) -> Option<(Vec<u8>, String)> {
    if let Some(local) =
        steam_local_art_bytes(appid, kind).or_else(|| steam_grid_art_bytes(appid, kind))
    {
        return Some(local);
    }
    // A non-Steam shortcut's appid has the high bit set (see [`shortcut_appid`]) and is never a real
    // store appid, so the CDN would only 404 — skip the wasted request and fall through cleanly.
    if appid & 0x8000_0000 != 0 {
        return None;
    }
    let url = format!(
        "https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}/{}",
        kind.cdn_filename()
    );
    fetch_image(&url)
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

/// Artwork a user set in Steam itself for a **non-Steam shortcut** (or a `grid/` override for a real
/// title), stored per-account under `userdata/<id>/config/grid/`, keyed by the same 32-bit appid the
/// shortcut carries. Tried before the CDN — a shortcut has no CDN art at all, so this is where its
/// poster lives.
fn steam_grid_art_bytes(appid: u32, kind: ArtKind) -> Option<(Vec<u8>, String)> {
    for root in steam_roots() {
        let Ok(users) = std::fs::read_dir(root.join("userdata")) else {
            continue;
        };
        for user in users.flatten() {
            let grid = user.path().join("config").join("grid");
            if let Some(path) = find_grid_art_file(&grid, appid, kind) {
                if let Ok(bytes) = std::fs::read(&path) {
                    let ctype = if path.extension().is_some_and(|e| e == "png") {
                        "image/png"
                    } else {
                        "image/jpeg"
                    };
                    return Some((bytes, ctype.to_string()));
                }
            }
        }
    }
    None
}

/// The `grid/` filenames Steam stores this art kind under for appid `<A>`, tried in order (PNG then
/// JPG — Steam accepts either). Pure path logic, so it's unit-testable against a directory fixture.
fn find_grid_art_file(grid: &Path, appid: u32, kind: ArtKind) -> Option<PathBuf> {
    for name in grid_filenames(kind, appid) {
        let path = grid.join(&name);
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > 0 && meta.len() <= LOCAL_ART_MAX_BYTES {
                return Some(path);
            }
        }
    }
    None
}

/// The `userdata/<id>/config/grid/` basenames Steam names each art kind under for appid `<A>`:
/// portrait `<A>p`, hero `<A>_hero`, logo `<A>_logo`, and the wide capsule `<A>` — each as `.png`
/// then `.jpg`.
fn grid_filenames(kind: ArtKind, appid: u32) -> Vec<String> {
    let both = |base: String| vec![format!("{base}.png"), format!("{base}.jpg")];
    match kind {
        ArtKind::Portrait => both(format!("{appid}p")),
        ArtKind::Hero => both(format!("{appid}_hero")),
        ArtKind::Logo => both(format!("{appid}_logo")),
        ArtKind::Header => both(format!("{appid}")),
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

/// One installed Steam title, as read from its `appmanifest_<appid>.acf`.
struct Installed {
    appid: u32,
    name: String,
    /// `<steamapps>/common/<installdir>`, when the manifest names one and it exists on disk — the
    /// game's own files, used to recognize its processes ([`DetectSpec::install_dir`]).
    install_dir: Option<PathBuf>,
}

/// Scan a `steamapps` dir for `appmanifest_*.acf` files → the installed titles it describes.
fn scan_manifests(steamapps: &Path) -> Vec<Installed> {
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
                // `installdir` is a bare folder name relative to this library's `common/`.
                let install_dir = text
                    .lines()
                    .find_map(|l| vdf_value(l.trim(), "installdir"))
                    .map(|d| steamapps.join("common").join(d))
                    .filter(|p| p.is_dir());
                out.push(Installed {
                    appid,
                    name: name.to_string(),
                    install_dir,
                });
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

/// One non-Steam shortcut ("Add a Non-Steam Game to My Library"), as read from `shortcuts.vdf`.
struct Shortcut {
    /// The 32-bit shortcut appid Steam assigns it (high bit set — see [`shortcut_appid`]). Keys the
    /// entry id and its `grid/` artwork; the 64-bit launch id derives from it ([`shortcut_gameid`]).
    appid: u32,
    /// Display name (`AppName`).
    name: String,
    /// The shortcut's target (`Exe`), as stored — Steam quotes it. This *is* the game (a shortcut
    /// points straight at it, with no launcher in between), so it doubles as the detect signal.
    exe: String,
    /// Whether Steam has this shortcut hidden from the library (`IsHidden`) — we honor that.
    hidden: bool,
}

/// Every non-Steam shortcut across all Steam accounts on this host, as launchable [`GameEntry`]s.
/// These carry no `appmanifest`, so [`scan_manifests`] never sees them — this is the only path that
/// surfaces a user's custom Steam entries. Best-effort: an unreadable/absent `shortcuts.vdf`
/// contributes nothing; hidden shortcuts and duplicate appids are dropped.
fn steam_shortcuts() -> Vec<GameEntry> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in shortcuts_files() {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        for sc in parse_shortcuts(&bytes) {
            if seen.insert(sc.appid) {
                if let Some(entry) = shortcut_entry(sc) {
                    out.push(entry);
                }
            }
        }
    }
    out
}

/// Map one parsed [`Shortcut`] to a library entry, or `None` if Steam has it hidden. Launch reuses
/// the `steam_appid` recipe (`steam steam://rungameid/<id>`) — the value is the 64-bit shortcut
/// game id, not the 32-bit appid, because the plain appid won't launch a non-Steam shortcut.
fn shortcut_entry(sc: Shortcut) -> Option<GameEntry> {
    if sc.hidden {
        return None;
    }
    Some(GameEntry {
        provider: None,
        role: GameRole::Game,
        meta: GameMeta::pc(),
        id: format!("steam:{}", sc.appid),
        store: "steam".into(),
        title: sc.name,
        art: steam_art(sc.appid),
        launch: Some(LaunchSpec {
            kind: "steam_appid".into(),
            value: shortcut_gameid(sc.appid).to_string(),
        }),
        detect: shortcut_detect(&sc.exe),
    })
}

/// Detect signals for a non-Steam shortcut: its `Exe` target is the game itself, so the executable
/// (and its folder, which catches a launcher script that execs a sibling binary) identifies it. Steam
/// stores the target quoted and may include trailing arguments; only an existing absolute path is
/// asserted — a guess would be worse than no tracking at all.
fn shortcut_detect(exe: &str) -> DetectSpec {
    let mut spec = crate::library::spec_from_command(exe);
    if let Some(dir) = spec.exe.as_deref().and_then(Path::parent) {
        spec.install_dir = Some(dir.to_path_buf());
    }
    spec
}

/// Every `userdata/<id>/config/shortcuts.vdf` under each Steam root — one file per Steam account
/// that has signed in on this host.
fn shortcuts_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in steam_roots() {
        let Ok(users) = std::fs::read_dir(root.join("userdata")) else {
            continue;
        };
        for user in users.flatten() {
            let path = user.path().join("config").join("shortcuts.vdf");
            if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}

// `shortcut_gameid` (the 64-bit `rungameid` composition) moved to `launch.rs` (WP1.1) — it is launch
// vocabulary; this module only reads the 32-bit appid out of `shortcuts.vdf`.

/// The 32-bit appid Steam derives for a shortcut from its target+name — `crc32(exe + name)` with the
/// high bit set. Only used when `shortcuts.vdf` omits the stored `appid` (very old Steam); modern
/// Steam writes it, and we prefer the stored value.
fn shortcut_appid(exe: &str, name: &str) -> u32 {
    crc32(format!("{exe}{name}").as_bytes()) | 0x8000_0000
}

/// Standard reflected (IEEE) CRC-32 — a few short strings' worth per scan, so a table-free bitwise
/// loop is plenty. Matches what Steam uses to hash a shortcut's `exe + name`.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Parse a **binary** `shortcuts.vdf` into its shortcuts. The format is Steam's binary KeyValues: a
/// 1-byte type tag (`0x00` nested map, `0x01` string, `0x02` int32), a NUL-terminated key, then a
/// type-specific payload; `0x08` closes the current map. The whole file is one `shortcuts` map whose
/// children (keyed `"0"`, `"1"`, …) are the individual shortcuts. Lenient and panic-free: a
/// truncated file or an unrecognized tag stops the walk and returns whatever parsed so far.
fn parse_shortcuts(buf: &[u8]) -> Vec<Shortcut> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    // Enter the top-level map (`<0x00> "shortcuts" <NUL>`); tolerate any key name.
    if buf.first() != Some(&0x00) {
        return out;
    }
    pos += 1;
    if read_cstr(buf, &mut pos).is_none() {
        return out;
    }
    // Each child is a map describing one shortcut, until the map-closing `0x08`.
    while let Some(&tag) = buf.get(pos) {
        pos += 1;
        if tag != 0x00 {
            break; // `0x08` (end of shortcuts) or anything unexpected
        }
        if read_cstr(buf, &mut pos).is_none() {
            break; // the index key ("0", "1", …)
        }
        match parse_one_shortcut(buf, &mut pos) {
            Some(sc) => out.push(sc),
            None => break,
        }
    }
    out
}

/// Parse one shortcut's fields (positioned just after its index key) up to the map-closing `0x08`,
/// pulling the ones we surface. `None` on a truncated/garbled entry.
fn parse_one_shortcut(buf: &[u8], pos: &mut usize) -> Option<Shortcut> {
    let mut appid: Option<u32> = None;
    let mut name = String::new();
    let mut exe = String::new();
    let mut hidden = false;
    loop {
        let tag = *buf.get(*pos)?;
        *pos += 1;
        if tag == 0x08 {
            break; // end of this shortcut
        }
        let key = read_cstr(buf, pos)?.to_ascii_lowercase();
        match tag {
            0x00 => skip_map(buf, pos)?, // nested map (e.g. `tags`) — not needed
            0x01 => {
                let val = read_cstr(buf, pos)?;
                match key.as_str() {
                    "appname" => name = val,
                    "exe" => exe = val,
                    _ => {}
                }
            }
            0x02 => {
                let val = read_i32(buf, pos)?;
                match key.as_str() {
                    "appid" => appid = Some(val as u32),
                    "ishidden" => hidden = val != 0,
                    _ => {}
                }
            }
            0x07 => *pos += 8, // uint64 — skip
            _ => return None,  // unknown tag: payload size unknown, can't continue safely
        }
    }
    if name.trim().is_empty() {
        return None; // nothing worth showing
    }
    // Prefer the stored appid; fall back to Steam's derivation when it's absent (0 / missing).
    let appid = appid
        .filter(|a| *a != 0)
        .unwrap_or_else(|| shortcut_appid(&exe, &name));
    Some(Shortcut {
        appid,
        name,
        exe,
        hidden,
    })
}

/// Skip a nested map's contents (positioned just after its key) up to and including its `0x08`.
fn skip_map(buf: &[u8], pos: &mut usize) -> Option<()> {
    loop {
        let tag = *buf.get(*pos)?;
        *pos += 1;
        if tag == 0x08 {
            return Some(());
        }
        read_cstr(buf, pos)?; // key
        match tag {
            0x00 => skip_map(buf, pos)?,
            0x01 => {
                read_cstr(buf, pos)?;
            }
            0x02 => *pos += 4,
            0x07 => *pos += 8,
            _ => return None,
        }
    }
}

/// Read a NUL-terminated UTF-8 string (lossy) starting at `pos`, advancing past the terminator.
/// `None` if the buffer ends before a NUL.
fn read_cstr(buf: &[u8], pos: &mut usize) -> Option<String> {
    let start = *pos;
    let end = buf.get(start..)?.iter().position(|&b| b == 0)? + start;
    let s = String::from_utf8_lossy(&buf[start..end]).into_owned();
    *pos = end + 1;
    Some(s)
}

/// Read a little-endian int32 at `pos`, advancing 4 bytes. `None` if fewer than 4 bytes remain.
fn read_i32(buf: &[u8], pos: &mut usize) -> Option<i32> {
    let bytes: [u8; 4] = buf.get(*pos..*pos + 4)?.try_into().ok()?;
    *pos += 4;
    Some(i32::from_le_bytes(bytes))
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

    // --- Non-Steam shortcuts (custom Steam entries) ---

    /// Build one binary-VDF field for a test `shortcuts.vdf`.
    fn field_str(key: &str, val: &str) -> Vec<u8> {
        let mut v = vec![0x01u8];
        v.extend_from_slice(key.as_bytes());
        v.push(0);
        v.extend_from_slice(val.as_bytes());
        v.push(0);
        v
    }
    fn field_i32(key: &str, val: i32) -> Vec<u8> {
        let mut v = vec![0x02u8];
        v.extend_from_slice(key.as_bytes());
        v.push(0);
        v.extend_from_slice(&val.to_le_bytes());
        v
    }
    fn map_open(key: &str) -> Vec<u8> {
        let mut v = vec![0x00u8];
        v.extend_from_slice(key.as_bytes());
        v.push(0);
        v
    }

    #[test]
    fn parse_shortcuts_reads_entries_honors_hidden_and_key_case() {
        let mut buf = Vec::new();
        buf.extend(map_open("shortcuts"));
        // Entry 0: a normal shortcut, with a nested `tags` map to exercise skip_map, and mixed-case
        // keys (Steam has shipped both `AppName` and `appname`). appid stored as a negative i32.
        buf.extend(map_open("0"));
        buf.extend(field_i32("appid", -1838178284)); // == 2456789012 as u32
        buf.extend(field_str("AppName", "My Emulator"));
        buf.extend(field_str("Exe", "\"/usr/bin/foo\""));
        buf.extend(field_i32("IsHidden", 0));
        buf.extend(map_open("tags"));
        buf.extend(field_str("0", "emulator"));
        buf.push(0x08); // end tags
        buf.push(0x08); // end entry 0
                        // Entry 1: hidden, lowercase key variant.
        buf.extend(map_open("1"));
        buf.extend(field_str("appname", "Hidden Game"));
        buf.extend(field_i32("ishidden", 1));
        buf.push(0x08); // end entry 1
        buf.push(0x08); // end shortcuts

        let scs = parse_shortcuts(&buf);
        assert_eq!(scs.len(), 2);
        assert_eq!(scs[0].appid, 2_456_789_012);
        assert_eq!(scs[0].name, "My Emulator");
        assert!(!scs[0].hidden);
        assert_eq!(scs[1].name, "Hidden Game");
        assert!(scs[1].hidden);

        // A hidden shortcut is dropped from the surfaced library; a visible one launches via its
        // 64-bit game id (not the bare appid).
        assert!(shortcut_entry(scs.into_iter().nth(1).unwrap()).is_none());
    }

    #[test]
    fn parse_shortcuts_is_lenient() {
        assert!(parse_shortcuts(b"").is_empty()); // not even a top-level map
        assert!(parse_shortcuts(b"{not binary vdf}").is_empty());
        // A truncated entry (buffer ends mid-int) yields what parsed cleanly before it — here, none.
        let mut buf = Vec::new();
        buf.extend(map_open("shortcuts"));
        buf.extend(map_open("0"));
        buf.extend_from_slice(b"\x02appid\x00\x01\x02"); // 2 of 4 int bytes, then EOF
        assert!(parse_shortcuts(&buf).is_empty());
    }

    #[test]
    fn shortcut_entry_launches_via_rungameid() {
        let sc = Shortcut {
            appid: 2_456_789_012,
            name: "My Emulator".into(),
            exe: "\"/opt/emu/run.sh\"".into(),
            hidden: false,
        };
        let entry = shortcut_entry(sc).unwrap();
        assert_eq!(entry.id, "steam:2456789012");
        assert_eq!(entry.store, "steam");
        let launch = entry.launch.unwrap();
        assert_eq!(launch.kind, "steam_appid");
        // Value is the 64-bit game id — digits only, so it passes the shared appid guard.
        assert_eq!(launch.value, shortcut_gameid(2_456_789_012).to_string());
        assert!(launch.value.bytes().all(|b| b.is_ascii_digit()));
    }

    // `shortcut_gameid_composes_appid_and_marker` moved with the function to `launch.rs` (WP1.1).

    #[test]
    fn crc32_matches_the_known_check_value_and_derives_a_high_bit_appid() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926); // IEEE CRC-32 check value
                                                      // A derived shortcut appid always has the high bit set (so it never collides with a real
                                                      // store appid, and its CDN art fetch is skipped).
        assert_ne!(
            shortcut_appid("\"/usr/bin/foo\"", "My Emulator") & 0x8000_0000,
            0
        );
    }

    #[test]
    fn grid_filenames_follow_steams_naming() {
        assert_eq!(
            grid_filenames(ArtKind::Portrait, 42),
            vec!["42p.png", "42p.jpg"]
        );
        assert_eq!(
            grid_filenames(ArtKind::Hero, 42),
            vec!["42_hero.png", "42_hero.jpg"]
        );
        assert_eq!(
            grid_filenames(ArtKind::Logo, 42),
            vec!["42_logo.png", "42_logo.jpg"]
        );
        assert_eq!(
            grid_filenames(ArtKind::Header, 42),
            vec!["42.png", "42.jpg"]
        );
    }

    #[test]
    fn find_grid_art_file_matches_the_userdata_grid_layout() {
        let grid = tempfile::tempdir().unwrap();
        std::fs::write(grid.path().join("2456789012p.jpg"), b"poster").unwrap();
        let found = find_grid_art_file(grid.path(), 2_456_789_012, ArtKind::Portrait).unwrap();
        assert_eq!(found, grid.path().join("2456789012p.jpg"));
        // Missing kinds and a zero-byte file both miss cleanly.
        assert_eq!(
            find_grid_art_file(grid.path(), 2_456_789_012, ArtKind::Hero),
            None
        );
        std::fs::write(grid.path().join("42p.png"), b"").unwrap();
        assert_eq!(find_grid_art_file(grid.path(), 42, ArtKind::Portrait), None);
    }
}
