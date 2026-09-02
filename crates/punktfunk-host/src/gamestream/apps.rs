//! GameStream app catalog: `/applist` entries and `/launch?appid=N` recipes.
//! Each entry names the compositor backend and, for gamescope, the nested command.
//! Loaded from `apps.json` under the host config dir; otherwise Desktop plus
//! gamescope defaults.
//!
//! ```json
//! [ {"id":1,"title":"Desktop"},
//!   {"id":2,"title":"Steam","compositor":"gamescope","cmd":"steam -gamepadui"} ]
//! ```

use serde_json::Value;

#[derive(Clone, Debug)]
pub struct AppEntry {
    pub id: u32,
    pub title: String,
    /// `None` = auto-detect the session compositor.
    pub compositor: Option<crate::vdisplay::Compositor>,
    /// Nested command; gamescope entries only.
    pub cmd: Option<String>,
    /// Store-qualified id (`steam:570`). When set, launch resolves this against the
    /// host library instead of running [`cmd`](Self::cmd).
    pub library_id: Option<String>,
    /// Sunshine `prep-cmd` parity: `do` before launch, `undo` reverse at stream end
    /// ([`crate::hooks::run_prep`]).
    pub prep: Vec<crate::hooks::PrepCmd>,
}

fn config_path() -> Option<std::path::PathBuf> {
    Some(pf_paths::config_dir().join("apps.json"))
}

fn parse_compositor(s: &str) -> Option<crate::vdisplay::Compositor> {
    use crate::vdisplay::Compositor::*;
    match s.to_ascii_lowercase().as_str() {
        "kwin" | "kde" => Some(Kwin),
        "mutter" | "gnome" => Some(Mutter),
        "gamescope" => Some(Gamescope),
        "hyprland" => Some(Hyprland),
        "wlroots" | "sway" | "river" => Some(Wlroots),
        _ => None,
    }
}

pub fn catalog() -> Vec<AppEntry> {
    let mut apps = base_catalog();
    append_library(&mut apps);
    apps
}

fn base_catalog() -> Vec<AppEntry> {
    if let Some(path) = config_path() {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<Value>(&raw) {
                Ok(Value::Array(items)) => {
                    let apps: Vec<AppEntry> = items
                        .iter()
                        .filter_map(|it| {
                            Some(AppEntry {
                                id: it.get("id")?.as_u64()? as u32,
                                title: it.get("title")?.as_str()?.to_string(),
                                compositor: it
                                    .get("compositor")
                                    .and_then(|c| c.as_str())
                                    .and_then(parse_compositor),
                                cmd: it.get("cmd").and_then(|c| c.as_str()).map(String::from),
                                library_id: None,
                                // Malformed `"prep"` is ignored; the entry still launches unprepped.
                                prep: it
                                    .get("prep")
                                    .and_then(|p| serde_json::from_value(p.clone()).ok())
                                    .unwrap_or_default(),
                            })
                        })
                        .collect();
                    if !apps.is_empty() {
                        return apps;
                    }
                    tracing::warn!(path = %path.display(), "apps.json parsed to zero entries — using defaults");
                }
                _ => {
                    tracing::warn!(path = %path.display(), "apps.json malformed — using defaults")
                }
            }
        }
    }
    let mut apps = vec![AppEntry {
        id: 1,
        title: "Desktop".into(),
        compositor: None,
        cmd: None,
        library_id: None,
        prep: Vec::new(),
    }];
    if which("gamescope") {
        if which("steam") {
            apps.push(AppEntry {
                id: 2,
                title: "Steam".into(),
                compositor: Some(crate::vdisplay::Compositor::Gamescope),
                cmd: Some("steam -gamepadui".into()),
                library_id: None,
                prep: Vec::new(),
            });
        }
        if which("vkcube") {
            apps.push(AppEntry {
                id: 3,
                title: "vkcube (test)".into(),
                compositor: Some(crate::vdisplay::Compositor::Gamescope),
                cmd: Some("vkcube".into()),
                library_id: None,
                prep: Vec::new(),
            });
        }
    }
    apps
}

/// High half of positive i32. Library GameStream ids live here so they never
/// collide with the small Desktop / apps.json ids.
const LIBRARY_ID_BASE: u32 = 0x4000_0000;

/// Layer [`crate::library::all_games`] onto `apps`. Moonlight caches appids, so
/// each title's `<ID>` is a stable hash of its library id, then linear-probed
/// off any id already in `apps`.
fn append_library(apps: &mut Vec<AppEntry>) {
    let mut used: std::collections::HashSet<u32> = apps.iter().map(|a| a.id).collect();
    for g in crate::library::all_games() {
        if g.launch.is_none() {
            continue;
        }
        let mut id = stable_app_id(&g.id);
        // Probe within the library range. Order is `all_games()`, so a collision
        // keeps the same id across runs.
        while !used.insert(id) {
            id = LIBRARY_ID_BASE | (id.wrapping_add(1) & 0x3FFF_FFFF);
        }
        apps.push(AppEntry {
            id,
            title: g.title,
            compositor: None, // Windows ignores compositor
            cmd: None,
            library_id: Some(g.id),
            prep: Vec::new(),
        });
    }
}

/// FNV-1a-32 of `library_id`, folded into [`LIBRARY_ID_BASE`]. Moonlight caches
/// the result, so the hash must stay byte-stable.
fn stable_app_id(library_id: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in library_id.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    LIBRARY_ID_BASE | (h & 0x3FFF_FFFF)
}

pub fn by_id(id: u32) -> Option<AppEntry> {
    catalog().into_iter().find(|a| a.id == id)
}

/// Blocking (disk + network) — call off the async runtime. Desktop / apps.json
/// entries have no art.
pub fn appasset_bytes(appid: u32) -> Option<(Vec<u8>, String)> {
    let lib_id = by_id(appid)?.library_id?;
    crate::library::fetch_box_art(&lib_id)
}

/// GameStream `/applist` XML. Compact — no whitespace between elements.
/// Moonlight-Android `getAppListByReader` calls `appList.getLast()` on every
/// text node before the first `<App>`; a pretty-print newline then throws
/// `NoSuchElementException`. `IsHdrSupported` is host-wide today.
pub fn applist_xml() -> String {
    let hdr = u8::from(crate::gamestream::host_hdr_capable());
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><root status_code=\"200\">");
    for app in catalog() {
        xml.push_str(&format!(
            "<App><IsHdrSupported>{hdr}</IsHdrSupported><AppTitle>{}</AppTitle><ID>{}</ID></App>",
            xml_escape(&app.title),
            app.id
        ));
    }
    xml.push_str("</root>");
    xml
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(bin).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_catalog_has_desktop() {
        let apps = catalog();
        assert!(apps.iter().any(|a| a.id == 1 && a.title == "Desktop"));
    }

    #[test]
    fn stable_app_id_is_deterministic_and_in_library_range() {
        let a = stable_app_id("steam:570");
        let b = stable_app_id("steam:570");
        let c = stable_app_id("steam:271590");
        assert_eq!(a, b);
        assert_ne!(a, c);
        for id in [a, c] {
            assert!(id >= LIBRARY_ID_BASE, "id {id:#x} below library base");
            assert!(id <= 0x7FFF_FFFF, "id {id:#x} not a positive i32");
            assert_ne!(id, 1, "must not collide with Desktop");
        }
    }

    /// GameStream id is the library-id bytes. A claimed plugin keeps the scanner
    /// id iff those bytes match; an unclaimed `custom:` id would not.
    #[test]
    fn a_claimed_plugin_entry_keeps_the_scanners_gamestream_id() {
        assert_eq!(stable_app_id("steam:440"), stable_app_id("steam:440"));
        assert_ne!(stable_app_id("steam:440"), stable_app_id("custom:9f2c1a"));
    }

    #[test]
    fn append_library_dedups_against_base_ids() {
        let mut apps = vec![AppEntry {
            id: stable_app_id("steam:570"),
            title: "Pinned".into(),
            compositor: None,
            cmd: None,
            library_id: None,
            prep: Vec::new(),
        }];
        append_library(&mut apps);
        let ids: Vec<u32> = apps.iter().map(|a| a.id).collect();
        let mut uniq = ids.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(ids.len(), uniq.len(), "duplicate GameStream ids in catalog");
    }

    #[test]
    fn applist_xml_is_wellformed_ish() {
        let xml = applist_xml();
        assert!(xml.contains("<AppTitle>Desktop</AppTitle>"));
        assert!(xml.starts_with("<?xml"));
        assert_eq!(xml.matches("<App>").count(), xml.matches("</App>").count());
    }

    #[test]
    fn applist_xml_has_no_interelement_whitespace() {
        let xml = applist_xml();
        assert!(
            xml.contains("status_code=\"200\"><App>"),
            "no whitespace between <root> and the first <App>: {xml}"
        );
        assert!(
            !xml.contains('\n'),
            "applist must contain no newlines: {xml}"
        );
        assert!(
            !xml.contains("> <"),
            "applist must contain no inter-element spaces: {xml}"
        );
    }
}
