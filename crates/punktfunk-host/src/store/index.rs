//! The plugin-store **index**: the catalog document a source serves, and the ed25519 signature
//! that makes it trustworthy (design `plugin-store.md` §3.2).
//!
//! The index is the verification gate. Each entry pins **one exact version plus that version's
//! tarball integrity hash** — so "verified on every release" is a property of the data, not a
//! promise about process: upstream can publish a newer version, but nothing in this host will
//! offer it until a re-reviewed entry lands in a signed index. There is deliberately no way to
//! express "track latest" for a catalogued plugin.
//!
//! Two rules keep parsing safe:
//! 1. **Signature before parse.** [`verify_signature`] runs over the exact bytes; only then does
//!    anything here look at a field. A source with a pinned key whose signature fails is an
//!    error, never a fallback to "unsigned".
//! 2. **Validate every field, drop what fails.** A malformed *entry* is dropped with a warning
//!    (one bad row shouldn't blind the operator to the rest of the shelf); a malformed
//!    *document* — unknown schema, non-JSON, oversized — is rejected whole.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The only index schema this host understands. A future breaking change bumps this and old
/// hosts report the source as unreadable rather than guessing at unknown semantics.
pub(crate) const SCHEMA: u32 = 1;

/// Hard cap on a fetched index body. Generous for a text catalog (the seed index is ~2 KB) and
/// small enough that a hostile or broken source can't exhaust memory.
pub(crate) const MAX_INDEX_BYTES: usize = 5 * 1024 * 1024;

/// Caps on how much of a (validly signed) index we'll keep. A curated shelf is small; these exist
/// so a compromised signing key can't turn the console into an unbounded list.
const MAX_PLUGINS: usize = 500;
const MAX_ADVISORIES: usize = 200;

// ---------------------------------------------------------------- the document

/// A source's catalog document, as served (and signed).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Index {
    /// Document schema — must equal [`SCHEMA`].
    pub schema: u32,
    /// Human-readable name of the catalog ("unom official"). Display only.
    #[serde(default)]
    pub name: String,
    /// RFC-3339 build timestamp. Display only — freshness is decided by our own fetch clock,
    /// never by a field the source controls.
    #[serde(default)]
    pub generated: String,
    /// The shelf.
    #[serde(default)]
    pub plugins: Vec<Entry>,
    /// Revocations/advisories, checked against **installed** packages as well as catalog rows.
    #[serde(default)]
    pub security: Vec<Advisory>,
}

/// One catalogued plugin: exactly one installable version, pinned by integrity hash.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    /// The plugin's `definePlugin` id (`[a-z][a-z0-9-]*`) — also how a running plugin appears in
    /// the lease registry, so the console can tell "catalogued" from "running".
    pub id: String,
    /// npm package name. **Must be scoped** (`@scope/name`): the scope is what maps the package
    /// to [`Entry::registry`] in bun's `bunfig.toml`, so an unscoped name would be ambiguous
    /// about where it installs from (design D8).
    pub pkg: String,
    /// The npm registry this package installs from. `https://` only.
    pub registry: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Lucide icon name for the console card (`[a-z0-9-]{1,48}`), matched against the console's
    /// curated set; anything unknown falls back to a generic puzzle piece.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// **The** installable version — exact, never a range. One version per entry: the index is a
    /// curated shelf, not a version archive (older releases live in the index repo's git history).
    pub version: String,
    /// npm integrity of that version's tarball (`sha512-<base64>`). Checked against the registry's
    /// own advertised integrity before any install runs — a republish under the same version with
    /// different content cannot pass.
    pub integrity: String,
    /// Present when a human reviewed this exact tarball. Only meaningful in the built-in source's
    /// index; a third-party source can write it, which is why it never grants the "Verified" badge
    /// (design D6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    /// Minimum host version this plugin supports (semver). Incompatible rows still list, greyed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_host: Option<String>,
    /// Host platforms this plugin works on (`linux`/`windows`/`macos`). Empty ⇒ all.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// What kinds of plugin this is (`[a-z][a-z0-9-]{0,31}`, ≤4). The console filters Browse by
    /// these, and the Game sources surface's "Add a source" rail lists exactly the entries carrying
    /// `library` (design D5/D6). Additive: an older host ignores the field, a newer one just sees no
    /// categories on an older index.
    #[serde(default)]
    pub categories: Vec<String>,
    /// Optional per-platform "is this launcher installed here?" probes (design D8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect: Option<DetectProbes>,
}

/// Existence probes that let the console badge a catalog row "detected on this host" **without the
/// host re-growing per-store knowledge** — the whole point of extracting the scanners. Store
/// knowledge lives in the updatable, signed index; the host stays generic and only evaluates.
///
/// Deliberately anaemic: a probe is a path or an `HKLM\…` registry key, checked for EXISTENCE only.
/// No reads, no content matching, no globbing beyond a single `*` segment. The index is
/// operator-trusted but remotely updatable, so a probe must never be able to exfiltrate anything or
/// cost more than a stat.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct DetectProbes {
    #[serde(default)]
    pub linux: Vec<String>,
    #[serde(default)]
    pub windows: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Verification {
    /// ISO date the review landed. Display only.
    pub reviewed_at: String,
}

/// A revocation: "this package at these versions is known-bad". Checked against what's *installed*,
/// not just what's listed — the whole point is to reach plugins already on the box.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Advisory {
    pub pkg: String,
    /// A semver requirement (`<0.3.2`, `>=1.0.0, <1.2.0`, `*`). Unparseable ⇒ the advisory is
    /// dropped rather than applied to everything.
    pub versions: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

// ---------------------------------------------------------------- signature

// Ed25519 verification moved to `pf-update-check` when the Linux client grew its own update
// check: the host's plugin-store index and both products' update manifests are all "verify a
// detached signature over exact bytes against pinned keys", and that rule must exist once.
// Re-exported under the old path so every call site here is unchanged.
pub(crate) use pf_update_check::sig::{verify_signature, PublicKey};

// ---------------------------------------------------------------- parse + validate

impl Index {
    /// Parse and validate an index document. Rejects the whole document on a structural problem;
    /// drops individual entries/advisories that fail validation (with a warning) so one bad row
    /// can't hide the rest of the catalog.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Index> {
        if bytes.len() > MAX_INDEX_BYTES {
            bail!("index is larger than the {MAX_INDEX_BYTES}-byte cap");
        }
        let mut idx: Index = serde_json::from_slice(bytes).context("index is not valid JSON")?;
        if idx.schema != SCHEMA {
            bail!(
                "unsupported index schema {} (this host understands {SCHEMA})",
                idx.schema
            );
        }
        idx.name = sanitize(&idx.name, 64);
        idx.generated = sanitize(&idx.generated, 40);

        let mut seen: Vec<String> = Vec::new();
        idx.plugins.truncate(MAX_PLUGINS);
        idx.plugins.retain_mut(|e| match e.validate() {
            Err(why) => {
                tracing::warn!(pkg = %e.pkg, "dropping catalog entry: {why}");
                false
            }
            Ok(()) => {
                // Duplicate ids would make "install this one" ambiguous; first wins.
                if seen.iter().any(|s| s == &e.id) {
                    tracing::warn!(id = %e.id, "dropping duplicate catalog entry");
                    false
                } else {
                    seen.push(e.id.clone());
                    true
                }
            }
        });

        idx.security.truncate(MAX_ADVISORIES);
        idx.security.retain_mut(|a| match a.validate() {
            Err(why) => {
                tracing::warn!(pkg = %a.pkg, "dropping advisory: {why}");
                false
            }
            Ok(()) => true,
        });
        Ok(idx)
    }
}

impl Entry {
    fn validate(&mut self) -> Result<()> {
        if !valid_plugin_id(&self.id) {
            bail!("id must be kebab-case `[a-z][a-z0-9-]*`, ≤64");
        }
        if !valid_scoped_pkg(&self.pkg) {
            bail!("pkg must be a scoped npm name (`@scope/name`)");
        }
        if !is_https(&self.registry) {
            bail!("registry must be an https:// URL");
        }
        self.title = sanitize(&self.title, 64);
        if self.title.is_empty() {
            bail!("title must not be empty");
        }
        self.description = sanitize(&self.description, 280);
        self.author = sanitize(&self.author, 64);
        self.version = sanitize(&self.version, 32);
        if semver::Version::parse(&self.version).is_err() {
            bail!("version must be exact semver (`1.2.3`), not a range");
        }
        if !valid_integrity(&self.integrity) {
            bail!("integrity must look like `sha512-<base64>`");
        }
        if let Some(icon) = &self.icon {
            if !valid_icon(icon) {
                self.icon = None; // cosmetic — drop it rather than the whole entry
            }
        }
        if let Some(h) = &self.homepage {
            if !is_https(h) || h.len() > 200 {
                self.homepage = None;
            }
        }
        if let Some(l) = &self.license {
            self.license = Some(sanitize(l, 64)).filter(|s| !s.is_empty());
        }
        if let Some(v) = &self.verification {
            if v.reviewed_at.len() > 32 {
                self.verification = None;
            }
        }
        if let Some(m) = &self.min_host {
            if semver::Version::parse(m).is_err() {
                self.min_host = None; // unusable constraint ⇒ no constraint
            }
        }
        self.platforms
            .retain(|p| matches!(p.as_str(), "linux" | "windows" | "macos"));
        self.platforms.truncate(4);
        // Categories and probes are cosmetic/advisory: a malformed one is dropped, never fatal to
        // the entry — a plugin must stay installable even if a future index writes a category this
        // host build has never heard of.
        self.categories.retain(|c| valid_category(c));
        self.categories.truncate(4);
        if let Some(d) = &mut self.detect {
            d.linux.retain(|p| valid_probe(p));
            d.windows.retain(|p| valid_probe(p));
            d.linux.truncate(MAX_PROBES);
            d.windows.truncate(MAX_PROBES);
            if d.linux.is_empty() && d.windows.is_empty() {
                self.detect = None;
            }
        }
        Ok(())
    }

    /// Does this entry's platform probe match on the running host? `None` = the entry declares no
    /// probes for this platform, i.e. "unknown", which the console renders differently from "no".
    pub(crate) fn detected(&self) -> Option<bool> {
        let probes = self.detect.as_ref()?;
        let list = if cfg!(windows) {
            &probes.windows
        } else if cfg!(target_os = "linux") {
            &probes.linux
        } else {
            return None;
        };
        if list.is_empty() {
            return None;
        }
        Some(list.iter().any(|p| probe_matches(p)))
    }

    /// Is this entry installable on the running host? Returns the operator-facing reason when not.
    pub(crate) fn incompatible_reason(&self) -> Option<String> {
        if !self.platforms.is_empty() && !self.platforms.iter().any(|p| p == HOST_PLATFORM) {
            return Some(format!("requires {}", self.platforms.join(" or ")));
        }
        if let Some(min) = &self.min_host {
            let (Ok(min), Ok(host)) = (
                semver::Version::parse(min),
                semver::Version::parse(host_version()),
            ) else {
                return None;
            };
            if host < min {
                return Some(format!("needs punktfunk {min} or newer"));
            }
        }
        None
    }
}

impl Advisory {
    fn validate(&mut self) -> Result<()> {
        if self.pkg.trim().is_empty() || self.pkg.len() > 214 {
            bail!("advisory pkg is empty or too long");
        }
        semver::VersionReq::parse(&self.versions)
            .context("advisory `versions` is not a semver requirement")?;
        self.reason = sanitize(&self.reason, 280);
        if let Some(u) = &self.url {
            if !is_https(u) || u.len() > 200 {
                self.url = None;
            }
        }
        Ok(())
    }

    /// Does this advisory cover `pkg@version`?
    pub(crate) fn matches(&self, pkg: &str, version: &str) -> bool {
        if self.pkg != pkg {
            return false;
        }
        let (Ok(req), Ok(v)) = (
            semver::VersionReq::parse(&self.versions),
            semver::Version::parse(version),
        ) else {
            return false;
        };
        req.matches(&v)
    }
}

// ---------------------------------------------------------------- small helpers

/// This host's platform token, as used in [`Entry::platforms`].
pub(crate) const HOST_PLATFORM: &str = if cfg!(target_os = "windows") {
    "windows"
} else if cfg!(target_os = "macos") {
    "macos"
} else {
    "linux"
};

/// The running host's version — the left-hand side of every `minHost` comparison.
pub(crate) fn host_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Strip control characters (a catalog string ends up in a log line and in the console UI) and
/// clamp to `max` characters.
fn sanitize(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

pub(crate) fn valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `@scope/name` with npm's safe alphabet. Deliberately stricter than npm: no URL-ish characters
/// can survive into a `bun add` argument or a `bunfig.toml` scope key.
pub(crate) fn valid_scoped_pkg(pkg: &str) -> bool {
    let Some(rest) = pkg.strip_prefix('@') else {
        return false;
    };
    if pkg.len() > 214 {
        return false;
    }
    let Some((scope, name)) = rest.split_once('/') else {
        return false;
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
            })
    };
    ok(scope) && ok(name)
}

/// The `@scope` half of a scoped package name — the key `bunfig.toml` maps to a registry.
pub(crate) fn scope_of(pkg: &str) -> Option<String> {
    let rest = pkg.strip_prefix('@')?;
    let (scope, _) = rest.split_once('/')?;
    Some(format!("@{scope}"))
}

fn valid_icon(icon: &str) -> bool {
    (1..=48).contains(&icon.len())
        && icon
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// npm's Subresource-Integrity spelling: `<alg>-<base64>`.
fn valid_integrity(s: &str) -> bool {
    if s.len() > 200 {
        return false;
    }
    let Some((alg, b64)) = s.split_once('-') else {
        return false;
    };
    matches!(alg, "sha512" | "sha384" | "sha256" | "sha1")
        && !b64.is_empty()
        && b64
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

fn is_https(url: &str) -> bool {
    url.starts_with("https://") && url.len() > "https://".len()
}

/// A plugin category (design D5): same shape the registration API accepts, so a plugin's declared
/// category and its catalog row can never disagree about spelling.
fn valid_category(c: &str) -> bool {
    (1..=32).contains(&c.len())
        && c.starts_with(|ch: char| ch.is_ascii_lowercase())
        && c.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// How many probes one platform may declare — a handful of well-chosen paths covers any launcher,
/// and the cap bounds the stat cost of rendering the catalog.
const MAX_PROBES: usize = 8;

/// Is this a probe the host will evaluate? An **absolute** filesystem path with at most one `*`
/// segment, or an `HKLM\…` registry key. Everything else is dropped.
///
/// The restrictions are the security model (D8). Absolute: a relative path would resolve against
/// whatever the host's cwd happens to be. One `*` segment: bounded fan-out, so a probe can't walk a
/// tree. `HKLM` only: `HKCU` is unreadable as LocalService anyway, and pointing the host at an
/// arbitrary hive is not something a remote index should be able to ask for.
fn valid_probe(p: &str) -> bool {
    if p.is_empty() || p.len() > 260 {
        return false;
    }
    if let Some(key) = p.strip_prefix("HKLM\\") {
        return !key.is_empty()
            && !key.contains("..")
            && key.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'\\' | b' ' | b'-' | b'_' | b'.')
            });
    }
    let b = p.as_bytes();
    let absolute = p.starts_with('/') || (b.len() >= 3 && b[1] == b':' && b[2] == b'\\');
    // No traversal, and at most ONE wildcard segment (`~` is not expanded — the host runs as a
    // service account whose home means nothing to a user's launcher install).
    absolute && !p.contains("..") && p.matches('*').count() <= 1
}

/// Evaluate one probe: does the path (or registry key) exist? Existence only — never a read.
fn probe_matches(p: &str) -> bool {
    #[cfg(windows)]
    if let Some(key) = p.strip_prefix("HKLM\\") {
        use std::os::windows::process::CommandExt;
        // `reg.exe query` rather than a registry crate: dependency-free, and it is exactly what a
        // library plugin will use for the same job under LocalService.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        return std::process::Command::new("reg.exe")
            .args(["query", &format!("HKLM\\{key}")])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    #[cfg(not(windows))]
    if p.starts_with("HKLM\\") {
        return false; // a Windows probe on a POSIX host is simply not a match
    }
    match p.split_once('*') {
        None => std::path::Path::new(p).exists(),
        // One wildcard: list the parent of the wildcard segment and match the fixed prefix/suffix
        // around it. Bounded to a single directory read.
        Some((before, after)) => {
            let (dir, prefix) = match before.rfind(['/', '\\']) {
                Some(i) => (&before[..=i], &before[i + 1..]),
                None => return false, // a wildcard with no directory to anchor it
            };
            let (suffix, rest) = match after.find(['/', '\\']) {
                Some(i) => (&after[..i], &after[i..]),
                None => (after, ""),
            };
            let Ok(read) = std::fs::read_dir(dir) else {
                return false;
            };
            read.flatten().any(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with(prefix)
                    && name.ends_with(suffix)
                    && name.len() >= prefix.len() + suffix.len()
                    && (rest.is_empty()
                        || e.path().join(rest.trim_start_matches(['/', '\\'])).exists())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(entry_json: &str) -> Vec<u8> {
        format!(r#"{{"schema":1,"name":"t","plugins":[{entry_json}]}}"#).into_bytes()
    }

    const GOOD: &str = r#"{"id":"rom-manager","pkg":"@punktfunk/plugin-rom-manager",
        "registry":"https://git.unom.io/api/packages/unom/npm/","title":"ROM Manager",
        "description":"d","author":"unom","version":"0.2.1","integrity":"sha512-AAAA",
        "verification":{"reviewedAt":"2026-07-19"},"platforms":["linux","windows"]}"#;

    #[test]
    fn parses_a_good_entry() {
        let idx = Index::parse(&doc(GOOD)).unwrap();
        assert_eq!(idx.plugins.len(), 1);
        let e = &idx.plugins[0];
        assert_eq!(e.pkg, "@punktfunk/plugin-rom-manager");
        assert_eq!(e.version, "0.2.1");
        assert!(e.verification.is_some());
    }

    #[test]
    fn rejects_unknown_schema_and_bad_json() {
        assert!(Index::parse(br#"{"schema":2,"plugins":[]}"#).is_err());
        assert!(Index::parse(b"not json").is_err());
    }

    /// WP2.8 is additive on purpose — SCHEMA stays 1. An index written by a newer curator must load
    /// on an older host (unknown fields ignored) and vice versa (absent fields default), or the
    /// signed-index rollout would need a flag day.
    #[test]
    fn categories_and_probes_are_additive_and_sanitized() {
        // An entry with NEITHER field — every index in the wild today.
        let e = &Index::parse(&doc(GOOD)).unwrap().plugins[0];
        assert!(e.categories.is_empty());
        assert!(e.detect.is_none());
        assert_eq!(e.detected(), None, "no probes ⇒ unknown, not `false`");

        // With both, including rows that must be dropped rather than fail the entry.
        let rich = GOOD.trim_end_matches('}').to_string()
            + r#","categories":["library","Bad Cat","x","y","z","w"],
                 "detect":{"linux":["/usr/bin/steam","relative/path","/etc/../etc/passwd"],
                           "windows":["HKLM\\SOFTWARE\\Valve\\Steam","HKCU\\SOFTWARE\\Valve"]}}"#;
        let e = &Index::parse(&doc(&rich)).unwrap().plugins[0];
        assert_eq!(
            e.categories,
            ["library", "x", "y", "z"],
            "malformed dropped, capped at 4"
        );
        let d = e.detect.as_ref().expect("probes kept");
        assert_eq!(d.linux, ["/usr/bin/steam"], "relative + traversal dropped");
        assert_eq!(
            d.windows,
            ["HKLM\\SOFTWARE\\Valve\\Steam"],
            "HKCU is not evaluable as LocalService — dropped"
        );
    }

    #[test]
    fn probe_shapes_are_bounded() {
        assert!(valid_probe("/usr/bin/steam"));
        assert!(
            valid_probe("/home/*/.steam"),
            "one wildcard segment is fine"
        );
        assert!(valid_probe(r"C:\Program Files (x86)\Steam\steam.exe"));
        assert!(valid_probe(r"HKLM\SOFTWARE\WOW6432Node\Valve\Steam"));
        // Rejected: relative, traversal, more than one wildcard, other hives, absurd length.
        assert!(!valid_probe("steam"));
        assert!(!valid_probe("/usr/../etc/passwd"));
        assert!(!valid_probe("/home/*/games/*/steam"));
        assert!(!valid_probe(r"HKCU\SOFTWARE\Valve"));
        assert!(!valid_probe(""));
        assert!(!valid_probe(&"/x".repeat(200)));
    }

    /// The evaluator does existence checks only, against real paths, and never reads a byte.
    #[test]
    fn probes_evaluate_against_the_filesystem() {
        let dir = std::env::temp_dir().join(format!("pf-probe-{}", std::process::id()));
        let nested = dir.join("SteamLibrary-42");
        std::fs::create_dir_all(nested.join("steamapps")).unwrap();
        let d = dir.to_string_lossy().into_owned();

        assert!(probe_matches(&format!("{d}/SteamLibrary-42")));
        assert!(!probe_matches(&format!("{d}/nope")));
        // One wildcard segment, with and without a trailing fixed component.
        assert!(probe_matches(&format!("{d}/SteamLibrary-*")));
        assert!(probe_matches(&format!("{d}/SteamLibrary-*/steamapps")));
        assert!(!probe_matches(&format!("{d}/SteamLibrary-*/nope")));
        assert!(!probe_matches(&format!("{d}/Other-*")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drops_invalid_entries_but_keeps_the_rest() {
        let bad_unscoped = GOOD.replace("@punktfunk/plugin-rom-manager", "punktfunk-plugin-x");
        let body = format!(r#"{{"schema":1,"plugins":[{bad_unscoped},{GOOD}]}}"#);
        let idx = Index::parse(body.as_bytes()).unwrap();
        assert_eq!(idx.plugins.len(), 1, "unscoped pkg must be dropped (D8)");
        assert_eq!(idx.plugins[0].id, "rom-manager");
    }

    #[test]
    fn rejects_version_ranges_and_http_registries() {
        assert!(Index::parse(&doc(
            &GOOD.replace(r#""version":"0.2.1""#, r#""version":"^0.2.1""#)
        ))
        .unwrap()
        .plugins
        .is_empty());
        assert!(Index::parse(&doc(
            &GOOD.replace("https://git.unom.io", "http://git.unom.io")
        ))
        .unwrap()
        .plugins
        .is_empty());
    }

    #[test]
    fn drops_duplicate_ids() {
        let body = format!(r#"{{"schema":1,"plugins":[{GOOD},{GOOD}]}}"#);
        assert_eq!(Index::parse(body.as_bytes()).unwrap().plugins.len(), 1);
    }

    #[test]
    fn sanitizes_control_characters_in_display_fields() {
        let e = GOOD.replace("ROM Manager", "RO\\u0007M");
        let idx = Index::parse(&doc(&e)).unwrap();
        assert_eq!(idx.plugins[0].title, "ROM");
    }

    #[test]
    fn advisory_matching_is_semver_ranged() {
        let mut a = Advisory {
            pkg: "@x/y".into(),
            versions: "<0.3.2".into(),
            reason: "bad".into(),
            url: None,
        };
        a.validate().unwrap();
        assert!(a.matches("@x/y", "0.3.1"));
        assert!(!a.matches("@x/y", "0.3.2"));
        assert!(!a.matches("@other/z", "0.3.1"));
    }

    #[test]
    fn advisory_with_unparseable_range_is_dropped() {
        let body = r#"{"schema":1,"plugins":[],"security":[
            {"pkg":"@x/y","versions":"not a range","reason":"r"}]}"#;
        assert!(Index::parse(body.as_bytes()).unwrap().security.is_empty());
    }

    #[test]
    fn scope_extraction() {
        assert_eq!(scope_of("@punktfunk/plugin-x").unwrap(), "@punktfunk");
        assert_eq!(scope_of("@a/b").unwrap(), "@a");
        assert!(scope_of("punktfunk-plugin-x").is_none());
    }

    #[test]
    fn integrity_shape() {
        assert!(valid_integrity("sha512-abcABC123+/="));
        assert!(!valid_integrity("md5-abc"));
        assert!(!valid_integrity("sha512-"));
        assert!(!valid_integrity("sha512"));
    }

    /// The real published index must parse here, field for field.
    ///
    /// This fixture is a snapshot of `unom/punktfunk-plugin-index`'s `v1/index.json`. The index
    /// repo's validator reimplements this module's rules in TypeScript (it has to — it gates PRs
    /// before anything is signed), and two hand-written parsers of the same grammar drift. This
    /// test is the seam that catches the drift from our side: if a document the publisher accepts
    /// stops being one we accept, an entry silently disappears from every operator's store.
    #[test]
    fn the_published_seed_index_parses() {
        let bytes = include_bytes!("testdata/seed-index.json");
        let idx = Index::parse(bytes).expect("the published index must parse");
        assert_eq!(idx.plugins.len(), 2, "no entry may be silently dropped");

        let rom = idx
            .plugins
            .iter()
            .find(|e| e.id == "rom-manager")
            .expect("rom-manager entry");
        assert_eq!(rom.pkg, "@punktfunk/plugin-rom-manager");
        assert!(rom.integrity.starts_with("sha512-"));
        assert!(
            semver::Version::parse(&rom.version).is_ok(),
            "exact version"
        );
        assert_eq!(
            rom.verification.as_ref().map(|v| v.reviewed_at.as_str()),
            Some("2026-07-20"),
            "camelCase `reviewedAt` must decode"
        );
        assert_eq!(
            rom.min_host.as_deref(),
            Some("0.15.0"),
            "camelCase `minHost`"
        );
        assert_eq!(scope_of(&rom.pkg).unwrap(), "@punktfunk");

        // A windows-only entry is listed everywhere but only *installable* where it can run.
        let playnite = idx
            .plugins
            .iter()
            .find(|e| e.id == "playnite")
            .expect("playnite entry");
        assert_eq!(playnite.platforms, vec!["windows"]);
        if HOST_PLATFORM == "windows" {
            assert!(playnite.incompatible_reason().is_none());
        } else {
            assert!(playnite.incompatible_reason().is_some());
        }
    }

    #[test]
    fn public_key_parsing_rejects_junk() {
        assert!(PublicKey::parse("nope").is_err());
        assert!(PublicKey::parse("ed25519:!!!").is_err());
        assert!(PublicKey::parse("ed25519:AAAA").is_err()); // wrong length
    }
}
