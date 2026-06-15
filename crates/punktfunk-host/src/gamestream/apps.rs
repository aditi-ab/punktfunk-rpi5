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
        "wlroots" | "sway" => Some(Wlroots),
        _ => None,
    }
}

/// The catalog: the user's `apps.json` if present, else defaults (Desktop, plus gamescope
/// entries when gamescope is installed).
pub fn catalog() -> Vec<AppEntry> {
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
    }];
    if which("gamescope") {
        if which("steam") {
            apps.push(AppEntry {
                id: 2,
                title: "Steam".into(),
                compositor: Some(crate::vdisplay::Compositor::Gamescope),
                cmd: Some("steam -gamepadui".into()),
            });
        }
        if which("vkcube") {
            apps.push(AppEntry {
                id: 3,
                title: "vkcube (test)".into(),
                compositor: Some(crate::vdisplay::Compositor::Gamescope),
                cmd: Some("vkcube".into()),
            });
        }
    }
    apps
}

pub fn by_id(id: u32) -> Option<AppEntry> {
    catalog().into_iter().find(|a| a.id == id)
}

/// Render the GameStream `/applist` XML.
pub fn applist_xml() -> String {
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">\n");
    for app in catalog() {
        xml.push_str(&format!(
            "<App>\n<IsHdrSupported>0</IsHdrSupported>\n<AppTitle>{}</AppTitle>\n<ID>{}</ID>\n</App>\n",
            xml_escape(&app.title),
            app.id
        ));
    }
    xml.push_str("</root>\n");
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
    fn applist_xml_is_wellformed_ish() {
        let xml = applist_xml();
        assert!(xml.contains("<AppTitle>Desktop</AppTitle>"));
        assert!(xml.starts_with("<?xml"));
        assert_eq!(xml.matches("<App>").count(), xml.matches("</App>").count());
    }
}
