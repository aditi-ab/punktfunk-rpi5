//! The signed **update manifest**: the check truth for "a newer build exists"
//! (design `host-update-from-web-console.md` §3).
//!
//! One small JSON document per channel, Ed25519-signed with keys pinned in the consuming
//! binary and verified by the exact code path the plugin store already trusts ([`crate::sig`]).
//! TLS and the registry that serves the document are transport, never trust.
//!
//! The document describes a *release*, not a product: the host and the Linux client ship from
//! one repo at one version, so both compare their own installed version against the same
//! `version`/`ci_run` here. Only the per-product payload legs (today: `windows_host`) are
//! product-specific, and a consumer that doesn't need one simply ignores it.
//!
//! Rules, all fail-closed (the sysext 303 lesson encoded):
//! 1. **Signature before parse** — over the exact fetched bytes, then strict JSON. An HTML
//!    error page, a redirect stub, or a truncated body dies before any field is read.
//! 2. **Channel binding** — the document names its channel and it must match the one we
//!    asked for, so a validly-signed canary manifest replayed onto the stable URL is refused.
//! 3. **Monotonic serial** — the publish-time serial can never go backwards for a channel
//!    (the anti-downgrade/anti-replay floor, persisted by the consumer).
//! 4. **Pinned notes origin** — the release-notes link a UI renders must live on our forge,
//!    so a signed-but-wrong document can't send the operator to a lookalike page.

use crate::sig::{verify_signature, PublicKey};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The only manifest schema this build understands. A breaking change bumps this; old builds
/// report "unsupported schema" instead of guessing.
pub const SCHEMA: u32 = 1;

/// Hard cap on a fetched manifest (and its signature). The real document is <1 KB.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// The only origin a manifest may point the operator at for release notes.
const NOTES_ORIGIN: &str = "https://git.unom.io/";

/// The signed update manifest, as served (and signed) per channel.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    /// Document schema — must equal [`SCHEMA`].
    pub schema: u32,
    /// The channel this document was published for (`stable` | `canary`). Bound-checked
    /// against the channel we fetched, see module docs rule 2.
    pub channel: String,
    /// Unix seconds at publish. Strictly increasing per channel; also drives the stale-feed
    /// hint (freshness needs no date parsing and no trust in `published_at`).
    pub serial: u64,
    /// RFC-3339 publish time. Display only.
    #[serde(default)]
    pub published_at: String,
    /// The released version this manifest announces.
    pub version: String,
    /// Release-notes link a UI renders. Must be on [`NOTES_ORIGIN`].
    #[serde(default)]
    pub notes_url: String,
    /// Canary only: the CI run number, the definitive "newer" axis where per-channel version
    /// strings differ (`~ciN`, `0.ciN`, a padded pkgrel, `M.m.run`).
    #[serde(default)]
    pub ci_run: Option<u64>,
    /// The Windows installer leg (design §6) — consumed by the host's Windows apply path and
    /// ignored everywhere else.
    #[serde(default)]
    pub windows_host: Option<WindowsHostAsset>,
}

/// Where the Windows host installer for [`Manifest::version`] lives and how to verify it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WindowsHostAsset {
    /// Immutable per-version download URL — never a mutable `latest/` alias, so the hash
    /// below can't race an alias re-upload.
    pub url: String,
    /// SHA-256 (hex) of the installer bytes.
    pub sha256: String,
    /// Accepted Authenticode signing-leaf SHA-256 fingerprints. Living in the signed manifest
    /// (not the binary) is what makes the self-signed → Trusted Signing migration a manifest
    /// edit instead of a lockstep host release.
    #[serde(default)]
    pub authenticode_sha256: Vec<String>,
    /// Minimum Windows build (display/preflight only).
    #[serde(default)]
    pub min_os: String,
}

/// Verify `sig_text` over the exact `bytes` against `keys`, then strictly parse and validate
/// the document for `expected_channel`. The only constructor — there is no unsigned path.
pub fn verify_and_parse(
    bytes: &[u8],
    sig_text: &str,
    keys: &[PublicKey],
    expected_channel: &str,
) -> Result<Manifest> {
    verify_signature(bytes, sig_text, keys).context("update manifest signature")?;
    parse_verified(bytes, expected_channel)
}

/// Parse + validate a document whose signature has already been checked. Split out so tests
/// can exercise validation without minting signatures for every case.
pub fn parse_verified(bytes: &[u8], expected_channel: &str) -> Result<Manifest> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        bail!("manifest is larger than the {MAX_MANIFEST_BYTES}-byte cap");
    }
    let m: Manifest = serde_json::from_slice(bytes).context("update manifest is not valid JSON")?;
    if m.schema != SCHEMA {
        bail!(
            "unsupported manifest schema {} (this build understands {SCHEMA})",
            m.schema
        );
    }
    if m.channel != expected_channel {
        bail!(
            "manifest is for channel `{}` but this build asked for `{expected_channel}`",
            m.channel
        );
    }
    if m.version.is_empty() || m.version.len() > 64 {
        bail!("manifest version is empty or implausibly long");
    }
    if m.serial == 0 {
        bail!("manifest serial is zero");
    }
    if !m.notes_url.is_empty() && !m.notes_url.starts_with(NOTES_ORIGIN) {
        bail!("manifest notes_url is not on {NOTES_ORIGIN}");
    }
    if let Some(w) = &m.windows_host {
        if !w.url.starts_with("https://") {
            bail!("windows_host.url must be https");
        }
        if w.sha256.len() != 64 || !w.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("windows_host.sha256 is not a hex SHA-256");
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "channel": "stable",
            "serial": 1785400000u64,
            "published_at": "2026-07-30T12:00:00Z",
            "version": "0.23.0",
            "notes_url": "https://git.unom.io/unom/punktfunk/releases/tag/v0.23.0",
            "windows_host": {
                "url": "https://git.unom.io/unom/punktfunk/releases/download/v0.23.0/punktfunk-host-setup-0.23.0.exe",
                "sha256": "aa".repeat(32),
                "authenticode_sha256": ["bb".repeat(32)],
                "min_os": "10.0.22621"
            }
        })
    }

    fn bytes(v: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(v).unwrap()
    }

    /// End-to-end: sign with a fresh ring keypair, verify with the matching pinned-key
    /// string — the format contract with the CI signer (raw 64-byte sig, base64; raw
    /// 32-byte key, `ed25519:<base64>`).
    #[test]
    fn signed_roundtrip_and_tamper() {
        use base64::Engine as _;
        let (key_str, kp) = crate::sig::tests::keypair();
        let keys = vec![PublicKey::parse(&key_str).unwrap()];

        let body = bytes(&doc());
        let sig = base64::engine::general_purpose::STANDARD.encode(kp.sign(&body));

        let m = verify_and_parse(&body, &sig, &keys, "stable").unwrap();
        assert_eq!(m.version, "0.23.0");
        assert_eq!(
            m.windows_host.as_ref().unwrap().authenticode_sha256.len(),
            1
        );

        // One flipped byte ⇒ refused before parse.
        let mut tampered = body.clone();
        tampered[10] ^= 1;
        assert!(verify_and_parse(&tampered, &sig, &keys, "stable").is_err());

        // Signed for stable, replayed as canary ⇒ refused (channel binding).
        assert!(verify_and_parse(&body, &sig, &keys, "canary").is_err());
    }

    #[test]
    fn html_error_page_is_not_a_manifest() {
        // The Gitea 303 stub that poisoned the sysext feed — must die in strict parse.
        let html = b"<a href=\"https://objects.example/x\">See Other</a>.";
        assert!(parse_verified(html, "stable").is_err());
    }

    #[test]
    fn truncated_json_refused() {
        let body = bytes(&doc());
        assert!(parse_verified(&body[..body.len() - 5], "stable").is_err());
    }

    #[test]
    fn wrong_schema_refused() {
        let mut v = doc();
        v["schema"] = serde_json::json!(2);
        assert!(parse_verified(&bytes(&v), "stable").is_err());
    }

    #[test]
    fn zero_serial_refused() {
        let mut v = doc();
        v["serial"] = serde_json::json!(0);
        assert!(parse_verified(&bytes(&v), "stable").is_err());
    }

    #[test]
    fn offsite_notes_url_refused() {
        let mut v = doc();
        v["notes_url"] = serde_json::json!("https://evil.example/notes");
        assert!(parse_verified(&bytes(&v), "stable").is_err());
    }

    #[test]
    fn windows_asset_validation() {
        let mut v = doc();
        v["windows_host"]["sha256"] = serde_json::json!("nothex");
        assert!(parse_verified(&bytes(&v), "stable").is_err());
        let mut v = doc();
        v["windows_host"]["url"] = serde_json::json!("http://git.unom.io/x.exe");
        assert!(parse_verified(&bytes(&v), "stable").is_err());
        // No windows leg at all is fine (PM channels don't need it).
        let mut v = doc();
        v.as_object_mut().unwrap().remove("windows_host");
        assert!(parse_verified(&bytes(&v), "stable").is_ok());
    }

    #[test]
    fn oversized_refused() {
        let mut v = doc();
        v["published_at"] = serde_json::json!("x".repeat(MAX_MANIFEST_BYTES));
        assert!(parse_verified(&bytes(&v), "stable").is_err());
    }
}
