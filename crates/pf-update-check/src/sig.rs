//! Detached Ed25519 signatures over an exact document (plugin-store index, update manifest).
//!
//! Moved here from `punktfunk-host::store::index` when the client grew its own update check:
//! one implementation, one set of tests, one place to fix if the format ever moves. The host
//! re-exports it, so its call sites are unchanged.

use anyhow::{bail, Context, Result};

/// An ed25519 public key pinned by a source record, spelled `ed25519:<base64 of the 32 raw bytes>`.
#[derive(Debug, Clone)]
pub struct PublicKey(Vec<u8>);

impl PublicKey {
    pub fn parse(s: &str) -> Result<Self> {
        use base64::Engine as _;
        let b64 = s
            .strip_prefix("ed25519:")
            .context("public key must be spelled `ed25519:<base64>`")?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .context("public key is not valid base64")?;
        if raw.len() != 32 {
            bail!("ed25519 public key must be 32 bytes, got {}", raw.len());
        }
        Ok(Self(raw))
    }
}

/// Verify a detached ed25519 signature over the **exact** document bytes against any of the
/// pinned keys (two slots, so a key rotation is "sign with the new one, ship a build that
/// trusts both, retire the old" rather than a flag day).
///
/// `sig_text` is the `.sig` file's contents: base64, whitespace-tolerant.
pub fn verify_signature(bytes: &[u8], sig_text: &str, keys: &[PublicKey]) -> Result<()> {
    use base64::Engine as _;
    if keys.is_empty() {
        bail!("no public key pinned for this source");
    }
    let sig = base64::engine::general_purpose::STANDARD
        .decode(sig_text.trim())
        .context("signature file is not valid base64")?;
    for key in keys {
        let pk =
            aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ED25519, &key.0);
        if pk.verify(bytes, &sig).is_ok() {
            return Ok(());
        }
    }
    bail!("signature does not verify against any pinned key")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh keypair as `(pinned key string, signer)` — the format contract with the
    /// CI signers (raw 32-byte key, `ed25519:<base64>`; raw 64-byte signature, base64).
    pub(crate) fn keypair() -> (String, aws_lc_rs::signature::Ed25519KeyPair) {
        use aws_lc_rs::signature::KeyPair as _;
        use base64::Engine as _;
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let key_str = format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(kp.public_key().as_ref())
        );
        (key_str, kp)
    }

    #[test]
    fn roundtrip_and_tamper() {
        use base64::Engine as _;
        let (key_str, kp) = keypair();
        let keys = vec![PublicKey::parse(&key_str).unwrap()];
        let body = b"the exact bytes";
        let sig = base64::engine::general_purpose::STANDARD.encode(kp.sign(body));

        assert!(verify_signature(body, &sig, &keys).is_ok());
        // A `.sig` file written by a shell redirect ends in a newline — tolerated.
        assert!(verify_signature(body, &format!("{sig}\n"), &keys).is_ok());
        assert!(verify_signature(b"other bytes", &sig, &keys).is_err());
        // No pinned key at all fails closed rather than skipping verification.
        assert!(verify_signature(body, &sig, &[]).is_err());
    }

    #[test]
    fn key_format_is_enforced() {
        assert!(PublicKey::parse("6rmlLg1aQ55cgB6icpC5BEpbMJxwPKdGaDQtDcJ0yLI=").is_err());
        assert!(PublicKey::parse("ed25519:not base64!!").is_err());
        // Right encoding, wrong length.
        assert!(PublicKey::parse("ed25519:AAAA").is_err());
    }

    /// A signature from a key we do NOT pin must fail even though it is perfectly valid —
    /// "verifies" and "verifies as us" are the same statement or the pinning is decorative.
    #[test]
    fn other_signer_refused() {
        use base64::Engine as _;
        let (ours, _) = keypair();
        let (_, theirs) = keypair();
        let keys = vec![PublicKey::parse(&ours).unwrap()];
        let body = b"the exact bytes";
        let sig = base64::engine::general_purpose::STANDARD.encode(theirs.sign(body));
        assert!(verify_signature(body, &sig, &keys).is_err());
    }
}
