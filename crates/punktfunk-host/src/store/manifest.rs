//! The **provenance manifest**: how a plugin got onto this box, remembered for as long as it is
//! installed (design `plugin-store.md` §4.5).
//!
//! `node_modules` knows *what* is installed; it has no idea whether the operator picked a reviewed
//! entry off the official shelf or pasted a tarball URL into the danger dialog. That distinction is
//! exactly what the console has to keep showing — an unverified plugin must stay visibly
//! unverified in the Installed list and in the header above its own UI, long after the install
//! dialog is forgotten.
//!
//! `<plugins_dir>/install-manifest.json`, written by the host (which is the only thing that ever
//! performs a *store* install). **Absence is meaningful**: a package with no manifest entry was
//! installed by the CLI, and is reported as [`Tier::Cli`] rather than being silently promoted.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How a plugin came to be installed. Ordered by how much review stands behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Tier {
    /// From the built-in source's index: unom reviewed this exact tarball.
    Verified,
    /// From an operator-added source's index: pinned and integrity-checked, but curated by
    /// somebody else. Attribution, never the badge (D6).
    External,
    /// Installed from a raw package spec through the danger dialog. Nobody reviewed it.
    Unverified,
    /// No manifest entry — `punktfunk-host plugins add` put it there.
    Cli,
}

impl Tier {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Tier::Verified => "verified",
            Tier::External => "external",
            Tier::Unverified => "unverified",
            Tier::Cli => "cli",
        }
    }
}

/// One remembered install.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Record {
    pub tier: Tier,
    /// The catalog source's slug, when the install came off a shelf.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The catalog entry id, when there was one — lets the console re-associate an installed
    /// package with its shelf row even if the package name is unusual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// The version we installed (as pinned). The *live* version always comes from disk; this is
    /// what we asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The raw spec, for [`Tier::Unverified`] installs — the only record of what the operator
    /// actually typed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    /// RFC-3339 install time (wall clock — display only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ManifestFile {
    schema: u32,
    /// Keyed by npm package name. `BTreeMap` so the file has a stable, diffable order.
    #[serde(default)]
    plugins: BTreeMap<String, Record>,
}

impl Default for ManifestFile {
    fn default() -> Self {
        Self {
            schema: 1,
            plugins: BTreeMap::new(),
        }
    }
}

fn manifest_path(plugins_dir: &std::path::Path) -> std::path::PathBuf {
    plugins_dir.join("install-manifest.json")
}

/// Read the manifest. A missing or corrupt file reads as empty — every package then reports as
/// CLI-installed, which is the honest answer when we have no provenance to show.
pub(crate) fn load(plugins_dir: &std::path::Path) -> BTreeMap<String, Record> {
    let Ok(bytes) = std::fs::read(manifest_path(plugins_dir)) else {
        return BTreeMap::new();
    };
    match serde_json::from_slice::<ManifestFile>(&bytes) {
        Ok(f) if f.schema == 1 => f.plugins,
        Ok(f) => {
            tracing::warn!(
                schema = f.schema,
                "unknown install-manifest schema — ignoring"
            );
            BTreeMap::new()
        }
        Err(e) => {
            tracing::warn!("install-manifest.json is unreadable ({e}) — treating as empty");
            BTreeMap::new()
        }
    }
}

/// Record (or replace) one package's provenance.
pub(crate) fn record(plugins_dir: &std::path::Path, pkg: &str, rec: Record) -> Result<()> {
    let mut plugins = load(plugins_dir);
    plugins.insert(pkg.to_string(), rec);
    write(plugins_dir, plugins)
}

/// Forget a package (on uninstall). Silent if it wasn't recorded.
pub(crate) fn forget(plugins_dir: &std::path::Path, pkg: &str) -> Result<()> {
    let mut plugins = load(plugins_dir);
    if plugins.remove(pkg).is_none() {
        return Ok(());
    }
    write(plugins_dir, plugins)
}

fn write(plugins_dir: &std::path::Path, plugins: BTreeMap<String, Record>) -> Result<()> {
    std::fs::create_dir_all(plugins_dir)
        .with_context(|| format!("create {}", plugins_dir.display()))?;
    let json = serde_json::to_string_pretty(&ManifestFile { schema: 1, plugins })
        .context("serialize install-manifest.json")?;
    let path = manifest_path(plugins_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{json}\n"))
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Wall-clock RFC-3339-ish stamp for [`Record::installed_at`]. Display only — nothing compares
/// these, so a clock jump costs a cosmetic oddity, never a decision.
pub(crate) fn now_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-date arithmetic from a Unix timestamp (days since epoch → y/m/d), so this needs no
    // date crate. UTC, second resolution.
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(tier: Tier) -> Record {
        Record {
            tier,
            source: Some("unom".into()),
            entry_id: Some("rom-manager".into()),
            version: Some("0.2.1".into()),
            spec: None,
            installed_at: Some(now_stamp()),
        }
    }

    #[test]
    fn round_trips_and_absence_means_cli() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty(), "no file ⇒ nothing recorded");

        record(
            dir.path(),
            "@punktfunk/plugin-rom-manager",
            rec(Tier::Verified),
        )
        .unwrap();
        let m = load(dir.path());
        let r = m.get("@punktfunk/plugin-rom-manager").unwrap();
        assert_eq!(r.tier, Tier::Verified);
        assert_eq!(r.version.as_deref(), Some("0.2.1"));
        // A package we never recorded has no entry — the caller reports it as CLI-installed.
        assert!(!m.contains_key("punktfunk-plugin-other"));
    }

    #[test]
    fn tier_survives_a_reinstall_at_a_new_tier() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "@x/y", rec(Tier::Verified)).unwrap();
        record(dir.path(), "@x/y", rec(Tier::Unverified)).unwrap();
        assert_eq!(load(dir.path()).get("@x/y").unwrap().tier, Tier::Unverified);
    }

    #[test]
    fn forget_removes_only_the_named_package() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "@x/y", rec(Tier::Verified)).unwrap();
        record(dir.path(), "@x/z", rec(Tier::External)).unwrap();
        forget(dir.path(), "@x/y").unwrap();
        let m = load(dir.path());
        assert!(!m.contains_key("@x/y"));
        assert!(m.contains_key("@x/z"));
        forget(dir.path(), "@not/here").unwrap(); // no-op, no error
    }

    #[test]
    fn corrupt_manifest_reads_as_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("install-manifest.json"), b"{ not json").unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn tier_json_spelling_is_stable() {
        // The console keys its badges off these strings.
        assert_eq!(
            serde_json::to_string(&Tier::Unverified).unwrap(),
            "\"unverified\""
        );
        assert_eq!(Tier::Verified.as_str(), "verified");
    }

    #[test]
    fn stamp_is_rfc3339_shaped() {
        let s = now_stamp();
        assert_eq!(s.len(), 20, "{s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
        // A known epoch value, to catch the civil-date arithmetic drifting.
        assert!(s.as_str() > "2026-01-01T00:00:00Z", "{s}");
    }
}
