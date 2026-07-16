//! The app catalog: what `/applist` advertises and what `/launch?appid=N` selects. Each entry
//! maps to a session recipe — which compositor backend hosts it and (for gamescope) which
//! command runs nested. Loaded from `~/.config/punktfunk/apps.json`; sensible defaults otherwise.
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
    /// `None` = auto-detect (the desktop session's compositor).
    pub compositor: Option<crate::vdisplay::Compositor>,
    /// Command gamescope runs nested (gamescope entries only).
    pub cmd: Option<String>,
    /// Store-qualified library id (`steam:570`, `epic:…`) for entries surfaced from the host's game
    /// library ([`crate::library`]). When set, the launch path resolves + launches it against the
    /// host's own library instead of running [`cmd`](Self::cmd). `None` for Desktop / apps.json entries.
    pub library_id: Option<String>,
    /// Per-app prep/undo steps (RFC §6, Sunshine `prep-cmd` parity): each `do` runs before the
    /// app launches, each `undo` at stream end in reverse order (see [`crate::hooks::run_prep`]).
    pub prep: Vec<crate::hooks::PrepCmd>,
}

fn config_path() -> Option<std::path::PathBuf> {
    // `config_dir()` resolves XDG/HOME on Linux and %APPDATA% on Windows (no HOME needed).
    Some(super::config_dir().join("apps.json"))
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

/// The GameStream catalog Moonlight sees in `/applist`: the operator base ([`base_catalog`] — Desktop +
/// apps.json) with the host's auto-detected game library ([`append_library`]) layered on top, so a
/// Moonlight client sees the same Steam/Epic/GOG/Xbox titles the native clients do instead of just Desktop.
pub fn catalog() -> Vec<AppEntry> {
    let mut apps = base_catalog();
    append_library(&mut apps);
    apps
}

/// The operator base: the user's `apps.json` if present, else defaults (Desktop, plus gamescope
/// entries when gamescope is installed). The installed game library is layered on by [`append_library`].
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
                                // `"prep": [{"do": …, "undo": …}, …]` — optional; a malformed
                                // array is ignored (the entry still launches, just unprepped).
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

/// The high half of the positive `i32` range — where library-derived GameStream ids live, kept clear of
/// the small Desktop/apps.json ids so the two never collide.
const LIBRARY_ID_BASE: u32 = 0x4000_0000;

/// Append the host's installed game library ([`crate::library::all_games`] — Steam/Epic/GOG/Xbox/custom)
/// to `apps`. Each title gets a STABLE GameStream `<ID>` derived from its store-qualified library id
/// (Moonlight caches appids, so a title keeps its id across host restarts), carries that library id so
/// the launch path resolves it against the host's own library, and is de-duplicated (by id) against the
/// base catalog and the other library entries. Titles with no launch recipe are skipped (un-startable).
fn append_library(apps: &mut Vec<AppEntry>) {
    let mut used: std::collections::HashSet<u32> = apps.iter().map(|a| a.id).collect();
    for g in crate::library::all_games() {
        if g.launch.is_none() {
            continue;
        }
        let mut id = stable_app_id(&g.id);
        // Linear-probe within the library range on the (rare) hash collision — deterministic given the
        // stable all_games() order, so a title keeps its id run to run.
        while !used.insert(id) {
            id = LIBRARY_ID_BASE | (id.wrapping_add(1) & 0x3FFF_FFFF);
        }
        apps.push(AppEntry {
            id,
            title: g.title,
            compositor: None, // auto-detect the desktop session (Windows ignores the compositor)
            cmd: None,
            library_id: Some(g.id),
            prep: Vec::new(),
        });
    }
}

/// A STABLE GameStream `<ID>` for a store-qualified library id (`steam:570`): FNV-1a-32 folded into the
/// high half of the positive `i32` range ([`LIBRARY_ID_BASE`]). Deterministic across runs and clear of
/// the reserved small Desktop/apps.json ids.
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

/// Box-art bytes for the GameStream `/appasset` cover proxy: resolve the Moonlight appid to its catalog
/// entry, then (for a library title) fetch its cover from the host's library. `(bytes, content-type)`,
/// or `None` for Desktop / apps.json entries (no art) or a fetch failure. Blocking (disk + network) —
/// call off the async runtime.
pub fn appasset_bytes(appid: u32) -> Option<(Vec<u8>, String)> {
    let lib_id = by_id(appid)?.library_id?;
    crate::library::fetch_box_art(&lib_id)
}

/// Render the GameStream `/applist` XML. `IsHdrSupported` reflects whether the host can actually deliver
/// HDR (HEVC Main10 / PQ) for a title — host-wide today ([`crate::gamestream::host_hdr_capable`]); when
/// true, Moonlight offers its per-app HDR toggle.
///
/// The document is emitted **COMPACT — no whitespace between elements** — deliberately, to match
/// Sunshine/GFE. Moonlight-Android's `getAppListByReader` calls `appList.getLast()` on *every* XML
/// text node before it checks the current tag, and only fills `appList` on an `<App>` start tag. A
/// pretty-print newline between `<root>` and the first `<App>` is a whitespace text node while
/// `appList` is still empty → `NoSuchElementException` → the Android app hard-crashes on host click.
/// (iOS/macOS parse via moonlight-common-c/expat and are unaffected; `serverinfo`/pairing use the
/// named-tag `getXmlString` scan, so their whitespace is harmless.) Keep this whitespace-free.
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
        // catalog() = base (Desktop + apps.json) + the installed library; Desktop (id 1) is always present.
        let apps = catalog();
        assert!(apps.iter().any(|a| a.id == 1 && a.title == "Desktop"));
    }

    #[test]
    fn stable_app_id_is_deterministic_and_in_library_range() {
        // Same id every run (Moonlight caches appids), distinct per title, and always in the high
        // half of the positive i32 range so it never collides with the small Desktop/apps.json ids.
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

    #[test]
    fn append_library_dedups_against_base_ids() {
        // A base app whose id happens to fall in the library range must not be clobbered by a library
        // entry that hashes to it — append_library probes past any used id.
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

    /// Regression: the applist MUST be whitespace-free between elements. Moonlight-Android's
    /// `getAppListByReader` calls `appList.getLast()` on every text node before an `<App>` has been
    /// pushed, so a pretty-print newline between `<root>` and the first `<App>` crashes the app
    /// (`NoSuchElementException`). Reproduced on 2 Android phones; iOS/macOS (moonlight-common-c)
    /// were unaffected. Keep `applist_xml` compact like Sunshine/GFE.
    #[test]
    fn applist_xml_has_no_interelement_whitespace() {
        let xml = applist_xml();
        // <root> is immediately followed by the first <App> — no whitespace text node while the
        // parser's app list is still empty.
        assert!(
            xml.contains("status_code=\"200\"><App>"),
            "no whitespace between <root> and the first <App>: {xml}"
        );
        // No pretty-print newlines anywhere in the element stream, and no whitespace-only text
        // nodes between any adjacent tags.
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
