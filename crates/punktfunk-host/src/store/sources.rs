//! Catalog **sources** — where the store's shelves come from (design `plugin-store.md` §3.1).
//!
//! One source = one URL serving a signed index. The **official** source is compiled in (URL plus
//! two pinned key slots) and cannot be edited or removed; operator-added sources live in
//! `<config_dir>/plugin-sources.json`.
//!
//! Adding a source is itself a trust decision, made once: a source's entries become installable
//! on this host. That is why they carry attribution and warning styling in the console but never
//! the "Verified" badge — verification is unom reviewing a specific tarball, and it is not
//! transferable to a third-party curator (D6).

use super::index::PublicKey;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The built-in source's slug. Reserved: an operator source may not take this name.
pub(crate) const OFFICIAL_NAME: &str = "unom";

/// The built-in source's index URL.
///
/// Served straight out of the index repo over Gitea's anonymous raw endpoint: real HTTPS with a
/// real certificate, no new vhost to stand up, and "merged to main" *is* "published". The document
/// is signed, so the transport is not what we're trusting — swapping this for a dedicated static
/// host later is a one-constant change with no protocol impact.
///
/// One consequence worth knowing: CI signs *after* the merge, so between a merge and the signature
/// commit the index is newer than its `.sig`. That window fails **closed** — the signature check
/// rejects the document and hosts keep serving their last good copy, marked stale.
pub(crate) const OFFICIAL_URL: &str =
    "https://git.unom.io/unom/punktfunk-plugin-index/raw/branch/main/v1/index.json";

/// Pinned signing keys for the built-in source. **Two slots** so a key rotation is "sign with the
/// new key, ship a host that trusts both, retire the old one" instead of a flag day where old
/// hosts lose the catalog. An empty slot is ignored.
pub(crate) const OFFICIAL_KEYS: [&str; 2] = [
    "ed25519:n/KgBQSSDZqvyGHa8PkHWHZtV1zRXLk0BdQUi4/BD/w=",
    "", // rotation slot
];

/// How many operator sources we'll hold. A guard rail, not a design limit.
const MAX_SOURCES: usize = 32;

/// A catalog source as stored in `plugin-sources.json`. camelCase like the index document it
/// points at (the management API's own view of a source is snake_case — see `mgmt::store`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Source {
    /// Slug (`[a-z][a-z0-9-]*`, ≤32) — the cache-file name and the console's attribution chip.
    pub name: String,
    /// `https://` URL of the index document. The `.sig` is fetched from `<url>.sig`.
    pub url: String,
    /// Pinned ed25519 key. A source without one is accepted but marked **unsigned**, and its
    /// entries inherit that marker — we can still show you the shelf, we just can't promise the
    /// shelf wasn't rewritten in transit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
}

impl Source {
    /// The compiled-in official source.
    pub(crate) fn official() -> Source {
        Source {
            name: OFFICIAL_NAME.to_string(),
            url: OFFICIAL_URL.to_string(),
            // Held in [`OFFICIAL_KEYS`], not here: the built-in keys are code, not config, so a
            // config file can never downgrade the official source to unsigned.
            public_key: None,
        }
    }

    pub(crate) fn is_official(&self) -> bool {
        self.name == OFFICIAL_NAME
    }

    /// The keys this source's index must verify against. Empty ⇒ unsigned source (accepted, but
    /// flagged); for the official source this is always the compiled-in pair.
    pub(crate) fn keys(&self) -> Vec<PublicKey> {
        if self.is_official() {
            return OFFICIAL_KEYS
                .iter()
                .filter(|k| !k.is_empty())
                .filter_map(|k| PublicKey::parse(k).ok())
                .collect();
        }
        self.public_key
            .as_deref()
            .and_then(|k| PublicKey::parse(k).ok())
            .into_iter()
            .collect()
    }

    /// Does this source's index carry a signature we check? Drives the console's "unsigned" marker.
    pub(crate) fn is_signed(&self) -> bool {
        !self.keys().is_empty()
    }

    /// Where the detached signature lives. Kept a pure function of the index URL so a source
    /// record can never point the signature fetch somewhere else than the document it signs.
    pub(crate) fn sig_url(&self) -> String {
        format!("{}.sig", self.url)
    }

    fn validate(&self) -> Result<()> {
        if !valid_source_name(&self.name) {
            bail!("source name must be kebab-case `[a-z][a-z0-9-]*`, ≤32 characters");
        }
        if !self.url.starts_with("https://") || self.url.len() > 500 {
            bail!("source url must be an https:// URL (≤500 characters)");
        }
        if let Some(k) = &self.public_key {
            PublicKey::parse(k).context("source publicKey")?;
        }
        Ok(())
    }
}

pub(crate) fn valid_source_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

// ---------------------------------------------------------------- persistence

#[derive(Serialize, Deserialize, Default)]
struct SourcesFile {
    #[serde(default = "one")]
    schema: u32,
    #[serde(default)]
    sources: Vec<Source>,
}

fn one() -> u32 {
    1
}

fn sources_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("plugin-sources.json")
}

/// Every source this host reads, official first. The official source is prepended here rather
/// than stored, so it survives a hand-edited (or deleted) config file.
pub(crate) fn load() -> Vec<Source> {
    let mut out = vec![Source::official()];
    let path = sources_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return out; // no operator sources yet — the common case
    };
    match serde_json::from_slice::<SourcesFile>(&bytes) {
        Ok(file) => {
            for s in file.sources.into_iter().take(MAX_SOURCES) {
                // Defense in depth: a hand-edited file can't smuggle in a source that shadows the
                // official one or carries an unusable URL/key.
                if s.is_official() || s.validate().is_err() {
                    tracing::warn!(name = %s.name, "ignoring invalid entry in plugin-sources.json");
                    continue;
                }
                if !out.iter().any(|e| e.name == s.name) {
                    out.push(s);
                }
            }
        }
        Err(e) => tracing::warn!(
            "plugin-sources.json is unreadable ({e}) — using the official source only"
        ),
    }
    out
}

/// Add or replace an operator source. Refuses the reserved official name and invalid records.
pub(crate) fn put(source: Source) -> Result<()> {
    source.validate()?;
    if source.is_official() {
        bail!("`{OFFICIAL_NAME}` is the built-in source and cannot be redefined");
    }
    let mut list: Vec<Source> = load().into_iter().filter(|s| !s.is_official()).collect();
    if list.len() >= MAX_SOURCES && !list.iter().any(|s| s.name == source.name) {
        bail!("too many plugin sources (max {MAX_SOURCES})");
    }
    list.retain(|s| s.name != source.name);
    list.push(source);
    save(list)
}

/// Remove an operator source. Returns `false` if there was nothing to remove.
pub(crate) fn remove(name: &str) -> Result<bool> {
    if name == OFFICIAL_NAME {
        bail!("the built-in `{OFFICIAL_NAME}` source cannot be removed");
    }
    let list: Vec<Source> = load().into_iter().filter(|s| !s.is_official()).collect();
    if !list.iter().any(|s| s.name == name) {
        return Ok(false); // nothing to remove — don't rewrite the file
    }
    save(list.into_iter().filter(|s| s.name != name).collect())?;
    Ok(true)
}

fn save(list: Vec<Source>) -> Result<()> {
    let dir = pf_paths::config_dir();
    pf_paths::create_private_dir(&dir).context("create the punktfunk config dir")?;
    let file = SourcesFile {
        schema: 1,
        sources: list,
    };
    let json = serde_json::to_string_pretty(&file).context("serialize plugin-sources.json")?;
    let path = sources_path();
    // Write-then-rename: a crash mid-write must not leave a half-file that makes the host forget
    // the operator's sources on next boot.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{json}\n"))
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_source_is_signed_by_compiled_in_keys() {
        let s = Source::official();
        assert!(s.is_official());
        assert!(s.is_signed(), "the built-in source must carry a pinned key");
        assert_eq!(s.sig_url(), format!("{OFFICIAL_URL}.sig"));
        // A config file cannot downgrade it: `keys()` ignores the record's own field entirely.
        let mut forged = Source::official();
        forged.public_key = Some("ed25519:AAAA".into());
        assert_eq!(forged.keys().len(), 1);
    }

    #[test]
    fn compiled_in_keys_parse() {
        for k in OFFICIAL_KEYS.iter().filter(|k| !k.is_empty()) {
            PublicKey::parse(k).expect("compiled-in official key must parse");
        }
    }

    #[test]
    fn unsigned_third_party_source_is_flagged_not_rejected() {
        let s = Source {
            name: "retro-hub".into(),
            url: "https://example.org/index.json".into(),
            public_key: None,
        };
        s.validate().unwrap();
        assert!(!s.is_signed());
    }

    #[test]
    fn validation_rejects_bad_names_urls_and_keys() {
        let base = Source {
            name: "ok".into(),
            url: "https://e.org/i.json".into(),
            public_key: None,
        };
        assert!(base.validate().is_ok());
        assert!(Source {
            name: "Bad".into(),
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(Source {
            name: "9bad".into(),
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(Source {
            url: "http://e.org/i.json".into(),
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(Source {
            public_key: Some("ed25519:nope".into()),
            ..base.clone()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn sig_url_is_derived_not_configurable() {
        let s = Source {
            name: "x".into(),
            url: "https://e.org/v1/index.json".into(),
            public_key: None,
        };
        assert_eq!(s.sig_url(), "https://e.org/v1/index.json.sig");
    }
}
