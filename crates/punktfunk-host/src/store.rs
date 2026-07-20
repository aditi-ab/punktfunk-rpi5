//! The **plugin store**: discovering and installing plugins from signed catalogs
//! (design `plugin-store.md`).
//!
//! Installing a plugin means running somebody's code with operator privileges, so the store is
//! built around one idea: *the index is the verification gate*. A catalog entry pins one exact
//! version and that version's tarball hash, and nothing here can express "track the latest
//! release". Upstream can publish whatever they like; this host will keep offering the reviewed
//! version until a newly reviewed entry lands in a signed index.
//!
//! That yields three tiers, which the whole surface is organized around:
//!
//! | tier | where it came from | surfaced? |
//! |------|--------------------|-----------|
//! | **verified**   | the built-in `unom` source's index — unom reviewed this exact tarball | yes, with a badge |
//! | **external**   | an operator-added source's index — pinned and integrity-checked, curated by somebody else | yes, attributed, no badge |
//! | **unverified** | a raw package spec typed into the console's danger dialog | never listed; install only |
//!
//! This module is the domain half (catalog state, installed-package facts, trust decisions); the
//! HTTP surface lives in [`crate::mgmt::store`]. Everything here is **blocking** — callers on the
//! async side hand it to `spawn_blocking`.

pub(crate) mod catalog;
pub(crate) mod index;
pub(crate) mod jobs;
pub(crate) mod manifest;
pub(crate) mod sources;

use anyhow::{bail, Result};
use index::{Advisory, Entry, Index};
use sources::Source;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// How long a fetched catalog is considered fresh. Catalogs change when somebody reviews a
/// release — hours, not seconds — and a streaming host should not be chatting to the internet on a
/// timer for a page nobody has open.
const CATALOG_TTL_SECS: u64 = 6 * 60 * 60;

/// Where plugin packages live: `<config_dir>/plugins` (the same dir the SDK and runner use).
pub(crate) fn plugins_dir() -> PathBuf {
    pf_paths::config_dir().join("plugins")
}

// ---------------------------------------------------------------- installed packages

/// A plugin package present in the plugins dir.
#[derive(Debug, Clone)]
pub(crate) struct InstalledPkg {
    pub pkg: String,
    pub version: Option<String>,
}

/// Enumerate installed plugin packages under `<dir>/node_modules`.
///
/// Mirrors the runner's own discovery so the store never claims something is installed that the
/// runner wouldn't supervise: the unscoped `punktfunk-plugin-*` convention, and **any** scope's
/// `plugin-*` (`@punktfunk/plugin-rom-manager`, `@retro-hub/plugin-x`). Scoped-any is what makes a
/// third-party catalog entry work at all — a scoped name is required for the registry mapping
/// (D8), so discovery must not be limited to the first-party scope.
pub(crate) fn installed_packages(dir: &Path) -> Vec<InstalledPkg> {
    let modules = dir.join("node_modules");
    let mut out = Vec::new();
    let version_of = |pkg_dir: &Path| -> Option<String> {
        let bytes = std::fs::read(pkg_dir.join("package.json")).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        v.get("version")?.as_str().map(str::to_string)
    };
    let Ok(entries) = std::fs::read_dir(&modules) else {
        return out; // nothing installed yet
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        if name.starts_with("punktfunk-plugin-") {
            let dir = modules.join(&name);
            out.push(InstalledPkg {
                version: version_of(&dir),
                pkg: name,
            });
        } else if name.starts_with('@') {
            let scope_dir = modules.join(&name);
            let Ok(scoped) = std::fs::read_dir(&scope_dir) else {
                continue;
            };
            let mut inner: Vec<String> = scoped
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            inner.sort();
            for s in inner {
                if s.starts_with("plugin-") {
                    let dir = scope_dir.join(&s);
                    out.push(InstalledPkg {
                        pkg: format!("{name}/{s}"),
                        version: version_of(&dir),
                    });
                }
            }
        }
    }
    out
}

/// Is `pkg` a name the runner would supervise? Guards the uninstall route so a stray
/// `POST /store/uninstall {"pkg": "effect"}` can't rip a shared dependency out of the tree.
pub(crate) fn valid_installed_pkg(pkg: &str) -> Result<()> {
    let plausible = pkg.starts_with("punktfunk-plugin-")
        || (index::valid_scoped_pkg(pkg)
            && pkg
                .split_once('/')
                .is_some_and(|(_, name)| name.starts_with("plugin-")));
    if !plausible {
        bail!("`{pkg}` is not a plugin package (`@scope/plugin-*` or `punktfunk-plugin-*`)");
    }
    Ok(())
}

/// The plugin id a package registers under (`@punktfunk/plugin-rom-manager` → `rom-manager`), used
/// to line an installed package up with the live lease registry. Best-effort: the authoritative id
/// for a catalogued plugin is its index entry.
pub(crate) fn plugin_id_for_pkg(pkg: &str) -> Option<String> {
    let last = pkg.rsplit('/').next()?;
    let id = last
        .strip_prefix("punktfunk-plugin-")
        .or_else(|| last.strip_prefix("plugin-"))?;
    index::valid_plugin_id(id).then(|| id.to_string())
}

// ---------------------------------------------------------------- catalog state

/// What we hold for one source: the last good index plus why it might be stale.
#[derive(Clone)]
pub(crate) struct SourceState {
    pub source: Source,
    pub index: Option<Index>,
    /// Unix seconds of the fetch that produced [`Self::index`].
    pub fetched_at: Option<u64>,
    /// True when the last refresh attempt failed and we're serving an older copy.
    pub stale: bool,
    /// Why the last attempt failed, for the console to show against the source.
    pub error: Option<String>,
    pub etag: Option<String>,
}

impl SourceState {
    fn empty(source: Source) -> Self {
        Self {
            source,
            index: None,
            fetched_at: None,
            stale: false,
            error: None,
            etag: None,
        }
    }

    fn is_fresh(&self) -> bool {
        self.index.is_some()
            && self
                .fetched_at
                .is_some_and(|t| catalog::unix_now().saturating_sub(t) < CATALOG_TTL_SECS)
    }
}

fn state() -> &'static RwLock<Vec<SourceState>> {
    static STATE: std::sync::OnceLock<RwLock<Vec<SourceState>>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| RwLock::new(Vec::new()))
}

/// Reconcile the in-memory state list with the configured sources (added/removed/edited), seeding
/// anything new from the on-disk cache so a cold host has a browsable store before its first fetch.
fn sync_sources() {
    let configured = sources::load();
    let dir = catalog::cache_dir();
    let mut st = state().write().unwrap_or_else(|e| e.into_inner());
    st.retain(|s| configured.iter().any(|c| c.name == s.source.name));
    for c in configured {
        match st.iter_mut().find(|s| s.source.name == c.name) {
            // The URL or key may have been edited under a name we already hold — drop what we
            // cached for the old definition rather than attributing it to the new one.
            Some(existing) => {
                if existing.source.url != c.url || existing.source.public_key != c.public_key {
                    *existing = SourceState::empty(c);
                } else {
                    existing.source = c;
                }
            }
            None => {
                let mut fresh = SourceState::empty(c.clone());
                if let Some((index, meta)) = catalog::read_cache(&dir, &c.name) {
                    fresh.index = Some(index);
                    fresh.fetched_at = Some(meta.fetched_at);
                    fresh.etag = meta.etag;
                    fresh.stale = true; // from disk — unverified freshness until a fetch says otherwise
                }
                st.push(fresh);
            }
        }
    }
}

/// Every source's catalog, refreshing those past their TTL (or all of them when `force`).
///
/// **Blocking** — network I/O. The freshness decision is ours (our fetch clock), never the
/// document's self-reported `generated` field.
pub(crate) fn catalogs(force: bool) -> Vec<SourceState> {
    sync_sources();
    let dir = catalog::cache_dir();
    let todo: Vec<Source> = {
        let st = state().read().unwrap_or_else(|e| e.into_inner());
        st.iter()
            .filter(|s| force || !s.is_fresh())
            .map(|s| s.source.clone())
            .collect()
    };
    for source in todo {
        let etag = {
            let st = state().read().unwrap_or_else(|e| e.into_inner());
            st.iter()
                .find(|s| s.source.name == source.name)
                .and_then(|s| s.etag.clone())
        };
        let outcome = catalog::fetch(&source, etag.as_deref());
        let now = catalog::unix_now();
        let mut st = state().write().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = st.iter_mut().find(|s| s.source.name == source.name) else {
            continue; // removed while we were fetching
        };
        match outcome {
            catalog::Fetched::Fresh { index, etag } => {
                let count = index.plugins.len();
                catalog::write_cache(
                    &dir,
                    &source.name,
                    &index,
                    &catalog::CacheMeta {
                        etag: etag.clone(),
                        fetched_at: now,
                    },
                );
                slot.index = Some(*index);
                slot.fetched_at = Some(now);
                slot.etag = etag;
                slot.stale = false;
                slot.error = None;
                tracing::info!(source = %source.name, entries = count, "plugin catalog refreshed");
            }
            catalog::Fetched::NotModified => {
                slot.fetched_at = Some(now);
                slot.stale = false;
                slot.error = None;
            }
            catalog::Fetched::Failed(why) => {
                // Fail soft: keep the last good shelf, say plainly that it's stale.
                tracing::warn!(source = %source.name, "plugin catalog refresh failed: {why}");
                slot.stale = true;
                slot.error = Some(why);
            }
        }
    }
    state()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// The catalogs we already hold, without touching the network. For paths that must not block on a
/// remote host (an install resolving its own entry, an advisory lookup).
pub(crate) fn cached_catalogs() -> Vec<SourceState> {
    sync_sources();
    state()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// Find a catalog entry by `(source, id)`, plus whether that source is the built-in one — which is
/// the single place "verified" is decided (D6).
pub(crate) fn find_entry(source_name: &str, id: &str) -> Option<(Entry, bool)> {
    cached_catalogs().into_iter().find_map(|s| {
        if s.source.name != source_name {
            return None;
        }
        let entry = s.index?.plugins.into_iter().find(|e| e.id == id)?;
        Some((entry, s.source.is_official()))
    })
}

/// The first advisory covering `pkg@version`, across every source we hold. Revocations are
/// deliberately not scoped to the source a plugin came from: a package known-bad anywhere is
/// known-bad here.
pub(crate) fn advisory_for(pkg: &str, version: Option<&str>) -> Option<Advisory> {
    let version = version?;
    cached_catalogs().into_iter().find_map(|s| {
        s.index?
            .security
            .into_iter()
            .find(|a| a.matches(pkg, version))
    })
}

/// Forget a removed source's cached shelf, so re-adding the name later can't serve stale rows.
pub(crate) fn drop_source_cache(name: &str) {
    catalog::drop_cache(&catalog::cache_dir(), name);
    let mut st = state().write().unwrap_or_else(|e| e.into_inner());
    st.retain(|s| s.source.name != name);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_pkg(root: &Path, pkg: &str, version: &str) {
        let dir = root.join("node_modules").join(pkg);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"{pkg}","version":"{version}"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn scans_both_conventions_and_any_scope() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "@punktfunk/plugin-rom-manager", "0.3.0");
        // A third-party scoped plugin MUST be found: catalog entries are required to be scoped
        // (D8), so limiting discovery to @punktfunk would make every external source unusable.
        touch_pkg(dir.path(), "@retro-hub/plugin-x", "1.0.0");
        touch_pkg(dir.path(), "punktfunk-plugin-legacy", "0.1.0");
        // Not plugins: a plain dependency and a scoped non-plugin.
        touch_pkg(dir.path(), "effect", "4.0.0");
        touch_pkg(dir.path(), "@punktfunk/host", "0.1.2");

        let found = installed_packages(dir.path());
        let names: Vec<&str> = found.iter().map(|p| p.pkg.as_str()).collect();
        assert!(
            names.contains(&"@punktfunk/plugin-rom-manager"),
            "{names:?}"
        );
        assert!(names.contains(&"@retro-hub/plugin-x"), "{names:?}");
        assert!(names.contains(&"punktfunk-plugin-legacy"), "{names:?}");
        assert!(!names.contains(&"effect"), "{names:?}");
        assert!(!names.contains(&"@punktfunk/host"), "{names:?}");
        assert_eq!(
            found
                .iter()
                .find(|p| p.pkg == "@punktfunk/plugin-rom-manager")
                .unwrap()
                .version
                .as_deref(),
            Some("0.3.0")
        );
    }

    #[test]
    fn scan_of_a_missing_dir_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(installed_packages(dir.path()).is_empty());
    }

    #[test]
    fn uninstall_target_must_be_a_plugin_package() {
        assert!(valid_installed_pkg("@punktfunk/plugin-rom-manager").is_ok());
        assert!(valid_installed_pkg("@retro-hub/plugin-x").is_ok());
        assert!(valid_installed_pkg("punktfunk-plugin-legacy").is_ok());
        // A shared dependency is not removable through the store.
        assert!(valid_installed_pkg("effect").is_err());
        assert!(valid_installed_pkg("@punktfunk/host").is_err());
        assert!(valid_installed_pkg("../../etc").is_err());
        assert!(valid_installed_pkg("").is_err());
    }

    #[test]
    fn plugin_id_derivation() {
        assert_eq!(
            plugin_id_for_pkg("@punktfunk/plugin-rom-manager").as_deref(),
            Some("rom-manager")
        );
        assert_eq!(
            plugin_id_for_pkg("punktfunk-plugin-playnite").as_deref(),
            Some("playnite")
        );
        assert_eq!(plugin_id_for_pkg("@a/plugin-x").as_deref(), Some("x"));
        assert_eq!(plugin_id_for_pkg("effect"), None);
    }
}
