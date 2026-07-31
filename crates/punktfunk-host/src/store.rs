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

use anyhow::{bail, Context, Result};
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

/// The packages the operator actually asked for: the `dependencies` of the plugins dir's own
/// `package.json`, which `bun add` maintains. `None` only when there is no readable `package.json`
/// at all.
///
/// This is what separates a plugin from a plugin's *library*. `@punktfunk/plugin-kit` is the
/// framework every kit-built plugin depends on — it matches the `plugin-*` naming convention
/// exactly, lands in `node_modules` as a transitive dependency, and is emphatically not something
/// the operator installed or can meaningfully uninstall. The convention alone cannot tell the two
/// apart; the top-level dependency list can.
///
/// A `package.json` with **no** `dependencies` key returns an empty list, not `None`: `bun remove`
/// drops the key entirely when the last plugin goes, and orphaned transitive packages can outlive
/// it in `node_modules`. Falling back to the naming convention there resurrects a plugin's library
/// as an installed plugin the moment you uninstall the last real one (seen on-glass). If bun is
/// managing this tree at all, its answer is the answer — including when the answer is "nothing".
fn top_level_deps(dir: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(match v.get("dependencies").and_then(|d| d.as_object()) {
        Some(deps) => deps.keys().cloned().collect(),
        None => Vec::new(),
    })
}

/// Enumerate installed plugin packages under `<dir>/node_modules`.
///
/// Mirrors the runner's own discovery so the store never claims something is installed that the
/// runner wouldn't supervise: the unscoped `punktfunk-plugin-*` convention, and **any** scope's
/// `plugin-*` (`@punktfunk/plugin-rom-manager`, `@retro-hub/plugin-x`). Scoped-any is what makes a
/// third-party catalog entry work at all — a scoped name is required for the registry mapping
/// (D8), so discovery must not be limited to the first-party scope.
///
/// Then narrowed to [`top_level_deps`] when a dependency list exists, so a plugin's *dependencies*
/// (notably `@punktfunk/plugin-kit`) aren't reported as installed plugins. A tree with no readable
/// `package.json` — hand-assembled, or an older layout — falls back to the convention alone rather
/// than reporting nothing.
pub(crate) fn installed_packages(dir: &Path) -> Vec<InstalledPkg> {
    let modules = dir.join("node_modules");
    let top_level = top_level_deps(dir);
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
    if let Some(top) = top_level {
        out.retain(|p| top.iter().any(|d| d == &p.pkg));
    }
    out
}

/// Point a package scope at its registry in the plugins dir's `bunfig.toml`.
///
/// The runner CLI can do this too (`--registry @scope=URL`), but the store must **not** depend on
/// that: `runner_command()` resolves whatever scripting package is installed on the box, which can
/// predate the host binary (the packaged runner and the host ship separately). An older runner
/// treats an unknown flag's *value* as a package name and the install dies with
/// "unrecognised dependency format" — found on-glass. Writing the mapping here keeps a
/// catalog-driven install working against every runner that has ever shipped.
///
/// Idempotent and non-destructive, matching `sdk/src/plugins.ts::ensureBunfig`: a scope already
/// mapped to this URL is left alone, one mapped elsewhere is rewritten, unrelated content survives.
pub(crate) fn ensure_bunfig_scope(dir: &Path, scope: &str, url: &str) -> Result<()> {
    // The scope and URL both come from a signature-verified, field-validated index entry
    // (`@`-prefixed, `[a-z0-9._-]`, https), so neither can smuggle a quote or newline into the TOML.
    if !index::valid_scoped_pkg(&format!("{scope}/x")) || !url.starts_with("https://") {
        bail!("refusing to map scope `{scope}` to `{url}`");
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("bunfig.toml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let wanted = format!("\"{scope}\" = \"{url}\"");

    let is_mapping_for_scope = |line: &str| {
        let t = line.trim_start();
        t.starts_with(&format!("\"{scope}\"")) || t.starts_with(&format!("{scope} "))
    };
    if existing.lines().any(|l| l.trim() == wanted) {
        return Ok(()); // already correct
    }
    let updated = if existing.lines().any(is_mapping_for_scope) {
        existing
            .lines()
            .map(|l| {
                if is_mapping_for_scope(l) {
                    wanted.clone()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    } else if let Some(pos) = existing
        .lines()
        .position(|l| l.trim() == "[install.scopes]")
    {
        let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
        lines.insert(pos + 1, wanted);
        lines.join("\n") + "\n"
    } else if existing.trim().is_empty() {
        format!("[install.scopes]\n{wanted}\n")
    } else {
        format!(
            "{}{}\n[install.scopes]\n{wanted}\n",
            existing,
            if existing.ends_with('\n') { "" } else { "\n" }
        )
    };
    std::fs::write(&path, updated).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Anchor bun's install root at the plugins dir by giving it a `package.json`.
///
/// **This is load-bearing.** `bun add` does not install into its working directory — it walks *up*
/// to the nearest ancestor `package.json` and installs into THAT tree. A fresh plugins dir has no
/// `package.json`, so any stray one above it (a `~/package.json` from someone's one-off `bun add`
/// or `npm init`) silently captures the install: bun reports success and **exits 0**, the packages
/// land in the ancestor's `node_modules`, and the plugins dir gets nothing. Reproduced on-glass
/// 2026-07-31 (Nobara 44, host + runner 0.22.3): the runner exits 0 in ~100 ms and the install then
/// fails the presence check with the unhelpful "not present after install" — a user report that
/// looked like a broken store and was a stray `~/package.json` all along.
///
/// The host writes this itself rather than leaving it to the runner, for the same reason as
/// [`ensure_bunfig_scope`]: the installed scripting package can be older than this binary.
///
/// Only seeds a tree that has no `node_modules` yet. A dir with packages but no `package.json` is
/// hand-assembled or an older layout, and [`installed_packages`] deliberately falls back to the
/// naming convention there — dropping an empty `dependencies` on it would make every plugin already
/// installed vanish from the store.
pub(crate) fn ensure_plugin_root(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("package.json");
    if path.exists() || dir.join("node_modules").exists() {
        return Ok(());
    }
    std::fs::write(
        &path,
        "{\n  \"name\": \"punktfunk-plugins\",\n  \"private\": true\n}\n",
    )
    .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// The nearest `package.json` **above** `dir`, if any — the thing that would capture a `bun add`
/// run inside `dir` (see [`ensure_plugin_root`]). Used to turn a silent mis-install into an error
/// the operator can act on.
pub(crate) fn capturing_ancestor(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .skip(1)
        .map(|a| a.join("package.json"))
        .find(|p| p.exists())
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

    /// A plugin's LIBRARY is not an installed plugin.
    ///
    /// Regression from a live install: `@punktfunk/plugin-kit` is the framework every kit-built
    /// plugin depends on. It matches the `plugin-*` convention exactly and lands in `node_modules`
    /// transitively, so a convention-only scan reported the framework as an installed plugin the
    /// operator could uninstall. The plugins dir's own `dependencies` is the authority.
    #[test]
    fn transitive_plugin_named_dependencies_are_not_installed_plugins() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "@punktfunk/plugin-rom-manager", "0.3.1");
        touch_pkg(dir.path(), "@punktfunk/plugin-kit", "0.1.3"); // a dependency of the above
        touch_pkg(dir.path(), "@punktfunk/host", "0.1.2");
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"@punktfunk/plugin-rom-manager":"^0.3.1"}}"#,
        )
        .unwrap();

        let found = installed_packages(dir.path());
        assert_eq!(
            found.iter().map(|p| p.pkg.as_str()).collect::<Vec<_>>(),
            vec!["@punktfunk/plugin-rom-manager"],
            "only the top-level install counts"
        );
    }

    #[test]
    fn a_tree_with_no_package_json_falls_back_to_the_convention() {
        // Hand-assembled or older layouts must still be discovered, not silently reported empty.
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "punktfunk-plugin-legacy", "0.1.0");
        assert_eq!(installed_packages(dir.path()).len(), 1);
    }

    /// A fresh plugins dir must own its install root, or `bun add` installs somewhere else.
    ///
    /// Field bug 2026-07-31: with no `package.json` here, bun walked up to the operator's stray
    /// `~/package.json`, installed the plugin into `~/node_modules`, and exited 0 — the store then
    /// failed the presence check on an empty dir.
    #[test]
    fn a_fresh_dir_is_seeded_as_its_own_install_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plugins");
        ensure_plugin_root(&root).unwrap();
        let seeded = std::fs::read_to_string(root.join("package.json")).unwrap();
        assert!(seeded.contains("punktfunk-plugins"), "{seeded}");
        // Seeding must not make the dir look like it has installs, nor lose the ones it gets.
        assert!(installed_packages(&root).is_empty());
    }

    #[test]
    fn seeding_never_touches_an_existing_package_json() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "@punktfunk/plugin-rom-manager", "0.3.1");
        let manifest = r#"{"dependencies":{"@punktfunk/plugin-rom-manager":"0.3.1"}}"#;
        std::fs::write(dir.path().join("package.json"), manifest).unwrap();
        ensure_plugin_root(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("package.json")).unwrap(),
            manifest
        );
    }

    /// The one tree we must NOT seed: packages present, no `package.json`. `installed_packages`
    /// falls back to the naming convention there, and a seeded empty `dependencies` would report
    /// every plugin the operator already runs as uninstalled (see
    /// `an_emptied_dependency_list_means_nothing_is_installed`).
    #[test]
    fn seeding_skips_a_tree_that_already_has_packages() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "punktfunk-plugin-legacy", "0.1.0");
        ensure_plugin_root(dir.path()).unwrap();
        assert!(!dir.path().join("package.json").exists());
        assert_eq!(installed_packages(dir.path()).len(), 1);
    }

    #[test]
    fn capturing_ancestor_looks_strictly_upwards() {
        let dir = tempfile::tempdir().unwrap();
        let plugins = dir.path().join("config/punktfunk/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(
            capturing_ancestor(&plugins),
            Some(dir.path().join("package.json"))
        );

        // The nearest one wins — that is the tree bun would pick.
        let nearer = dir.path().join("config/package.json");
        std::fs::write(&nearer, "{}").unwrap();
        assert_eq!(capturing_ancestor(&plugins), Some(nearer));

        // The dir's OWN manifest is not a capture — it is the anchor that prevents one.
        std::fs::write(plugins.join("package.json"), "{}").unwrap();
        assert_ne!(
            capturing_ancestor(&plugins),
            Some(plugins.join("package.json"))
        );
    }

    /// Uninstalling the last plugin must not resurrect its library as an installed plugin.
    ///
    /// `bun remove` drops the `dependencies` key entirely once it empties, while orphaned
    /// transitive packages can linger in `node_modules`. Treating "package.json exists but has no
    /// dependencies" as "no authority, fall back to the naming convention" made
    /// `@punktfunk/plugin-kit` pop back into the installed list right after the operator removed
    /// the only real plugin — seen on-glass.
    #[test]
    fn an_emptied_dependency_list_means_nothing_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "@punktfunk/plugin-kit", "0.1.3"); // orphan left behind
        std::fs::write(dir.path().join("package.json"), r#"{"name":"plugins"}"#).unwrap();
        assert!(installed_packages(dir.path()).is_empty());

        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"plugins","dependencies":{}}"#,
        )
        .unwrap();
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
    fn bunfig_scope_mapping_is_idempotent_and_preserves_other_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let read = || std::fs::read_to_string(dir.path().join("bunfig.toml")).unwrap();

        ensure_bunfig_scope(dir.path(), "@retro-hub", "https://retro.example/npm/").unwrap();
        assert!(read().contains("[install.scopes]"));
        assert!(read().contains("\"@retro-hub\" = \"https://retro.example/npm/\""));

        // Idempotent — no duplicate line.
        ensure_bunfig_scope(dir.path(), "@retro-hub", "https://retro.example/npm/").unwrap();
        assert_eq!(read().matches("@retro-hub").count(), 1);

        // A second scope joins the same table.
        ensure_bunfig_scope(
            dir.path(),
            "@punktfunk",
            "https://git.unom.io/api/packages/unom/npm/",
        )
        .unwrap();
        assert!(read().contains("@punktfunk"));
        assert!(read().contains("@retro-hub"));

        // A changed registry rewrites in place rather than duplicating.
        ensure_bunfig_scope(dir.path(), "@retro-hub", "https://new.example/npm/").unwrap();
        assert_eq!(read().matches("@retro-hub").count(), 1);
        assert!(read().contains("https://new.example/npm/"));
        assert!(!read().contains("retro.example"));
        assert!(read().contains("@punktfunk"), "unrelated scope survives");
    }

    #[test]
    fn bunfig_scope_mapping_refuses_junk() {
        let dir = tempfile::tempdir().unwrap();
        // Only https, only a well-formed scope — both already guaranteed by index validation, held
        // here as the second line of defence for the one place we format TOML by hand.
        assert!(ensure_bunfig_scope(dir.path(), "@x", "http://insecure/").is_err());
        assert!(ensure_bunfig_scope(dir.path(), "no-at-sign", "https://e/").is_err());
        assert!(ensure_bunfig_scope(dir.path(), "@bad\"quote", "https://e/").is_err());
        assert!(!dir.path().join("bunfig.toml").exists());
    }

    /// The name-shape guard is necessary but NOT sufficient — see `mgmt::store::uninstall_plugin`.
    ///
    /// `@punktfunk/plugin-kit` is a plugin's *framework*, and it satisfies every syntactic rule
    /// here. Windows on-glass accepted an uninstall of it. The real gate is membership in
    /// [`installed_packages`], which excludes transitive dependencies; this test pins the fact that
    /// the shape check alone lets it through, so nobody "simplifies" the handler back.
    #[test]
    fn name_shape_alone_does_not_protect_a_plugins_framework() {
        assert!(
            valid_installed_pkg("@punktfunk/plugin-kit").is_ok(),
            "shape check passes plugin-kit — the handler must additionally require that the \
             package is a top-level install"
        );

        let dir = tempfile::tempdir().unwrap();
        touch_pkg(dir.path(), "@punktfunk/plugin-rom-manager", "0.3.1");
        touch_pkg(dir.path(), "@punktfunk/plugin-kit", "0.1.3");
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"@punktfunk/plugin-rom-manager":"0.3.1"}}"#,
        )
        .unwrap();
        let installed = installed_packages(dir.path());
        assert!(installed
            .iter()
            .any(|p| p.pkg == "@punktfunk/plugin-rom-manager"));
        assert!(
            !installed.iter().any(|p| p.pkg == "@punktfunk/plugin-kit"),
            "the framework is not an installed plugin, so uninstall must refuse it"
        );
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
