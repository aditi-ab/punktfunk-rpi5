//! Native-plane host identity: the ECDSA P-256 PEM pair the QUIC plane and the
//! management API present together.
//!
//! Clients TOFU-pin the SHA-256 of the leaf DER at pairing and reuse that pin
//! for both planes, so the identity must not rotate under a live pin. GameStream
//! keeps its own RSA identity (`gamestream::cert`); Moonlight pairing hashes
//! bind those X.509 signature bytes. P-256, not Ed25519: no mainstream browser
//! accepts an Ed25519 server cert.
//!
//! [`load_or_adopt`] is the only writer:
//! * `native-cert.pem` + `native-key.pem` exist → use them;
//! * empty native trust store → mint P-256, persist, adopt;
//! * otherwise live native pairings → keep serving the legacy RSA pair those
//!   clients pinned, and log how to migrate (unpair all, restart, re-pair).
//!
//! Tests in this module pin the three branches.

use anyhow::{Context, Result};
use pf_paths::config_dir;
use std::fs;

/// PEM pair both native consumers present. Parsed generically, so RSA
/// (legacy fallback) and P-256 both fit.
#[derive(Clone)]
pub struct NativeIdentity {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Resolve the native identity per the module-docs migration rule.
/// Call once per process, before either plane starts: two concurrent
/// callers can race the first-run file writes.
pub fn load_or_adopt(np: &crate::native_pairing::NativePairing) -> Result<NativeIdentity> {
    let dir = config_dir();
    let cert_path = dir.join("native-cert.pem");
    let key_path = dir.join("native-key.pem");
    // Lock the config dir down before the first read so a pre-planted
    // cert/key pair cannot be adopted out of a user-writable directory.
    pf_paths::create_private_dir(&dir).ok();
    if let (Ok(c), Ok(k)) = (
        fs::read_to_string(&cert_path),
        fs::read_to_string(&key_path),
    ) {
        if !c.trim().is_empty() && !k.trim().is_empty() {
            return Ok(NativeIdentity {
                cert_pem: c,
                key_pem: k,
            });
        }
    }
    if np.list().is_empty() {
        let (cert_pem, key_pem) = generate()?;
        // Trust-root key: owner-only. Same write path as the cert so first
        // persist cannot leave a world-readable key.
        pf_paths::write_secret_file(&key_path, key_pem.as_bytes())
            .with_context(|| format!("write {}", key_path.display()))?;
        pf_paths::write_secret_file(&cert_path, cert_pem.as_bytes())
            .with_context(|| format!("write {}", cert_path.display()))?;
        tracing::info!(
            path = %cert_path.display(),
            "generated the native host identity (ECDSA P-256, SANs, key 0600)"
        );
        return Ok(NativeIdentity { cert_pem, key_pem });
    }
    // Live native pairings pinned the legacy RSA leaf (SHA-256 of DER).
    // Switching now strands them. PEM-only read: rustls can serve RSA
    // without linking the `rsa` crate (that crate stays behind `gamestream`).
    if let (Ok(c), Ok(k)) = (
        fs::read_to_string(dir.join("cert.pem")),
        fs::read_to_string(dir.join("key.pem")),
    ) {
        if !c.trim().is_empty() && !k.trim().is_empty() {
            tracing::info!(
                "native identity: keeping the legacy RSA cert — paired native clients pinned it. \
                 To migrate to the P-256 identity: unpair ALL native clients, restart the host, \
                 re-pair."
            );
            return Ok(NativeIdentity {
                cert_pem: c,
                key_pem: k,
            });
        }
    }
    tracing::warn!(
        "native identity: paired native clients exist but the legacy cert.pem/key.pem they \
         pinned is missing — minting the P-256 identity; those clients must re-pair"
    );
    let (cert_pem, key_pem) = generate()?;
    pf_paths::write_secret_file(&key_path, key_pem.as_bytes())
        .with_context(|| format!("write {}", key_path.display()))?;
    pf_paths::write_secret_file(&cert_path, cert_pem.as_bytes())
        .with_context(|| format!("write {}", cert_path.display()))?;
    Ok(NativeIdentity { cert_pem, key_pem })
}

/// In-memory identity for tests; does not touch the config dir.
#[cfg(test)]
pub fn ephemeral() -> Result<NativeIdentity> {
    let (cert_pem, key_pem) = generate()?;
    Ok(NativeIdentity { cert_pem, key_pem })
}

/// Mint a P-256 leaf. SANs are names a browser or loopback poller
/// actually dials; LAN IPs are omitted because they change and native
/// clients pin the fingerprint rather than verify names.
fn generate() -> Result<(String, String)> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .context("generate P-256 host key")?;
    let mut sans = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    let hn = crate::gamestream::machine_hostname();
    if !hn.is_empty()
        && hn.len() <= 63
        && hn
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        sans.insert(0, hn);
    }
    let mut params = rcgen::CertificateParams::new(sans).context("cert params")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "punktfunk");
    // Fixed 2020–2040 window, matching the legacy identity: no clock-skew
    // surprises, and clients pin the fingerprint rather than check validity.
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2040, 1, 1);
    let cert = params.self_signed(&key).context("self-sign cert")?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Serializes every test in this crate that overrides `PUNKTFUNK_CONFIG_DIR`.
/// The env var is process-global: parallel tests would share throwaway
/// config dirs. Lock it before installing the override.
#[cfg(test)]
pub(crate) static CONFIG_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Scoped `PUNKTFUNK_CONFIG_DIR` override, restored on drop even if an
    /// assertion panics. These tests write identity files and must never
    /// touch a real config dir.
    struct EnvGuard(Option<std::ffi::OsString>);
    impl EnvGuard {
        fn set(dir: &std::path::Path) -> EnvGuard {
            let prev = std::env::var_os("PUNKTFUNK_CONFIG_DIR");
            // SAFETY: only called by tests that hold CONFIG_DIR_TEST_LOCK, which serializes
            // every test that writes or reads this variable across the whole test binary.
            unsafe { std::env::set_var("PUNKTFUNK_CONFIG_DIR", dir) };
            EnvGuard(prev)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.0.take() {
                // SAFETY: dropped while the owning test still holds CONFIG_DIR_TEST_LOCK — the
                // same serialization as `EnvGuard::set`.
                Some(v) => unsafe { std::env::set_var("PUNKTFUNK_CONFIG_DIR", v) },
                // SAFETY: as above.
                None => unsafe { std::env::remove_var("PUNKTFUNK_CONFIG_DIR") },
            }
        }
    }

    fn empty_store(dir: &std::path::Path) -> crate::native_pairing::NativePairing {
        crate::native_pairing::NativePairing::load_with(
            Some(dir.join("native-trust.json")),
            None,
            false,
        )
        .unwrap()
    }

    #[test]
    fn adopts_p256_when_no_client_ever_pinned() {
        let _serial = super::CONFIG_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set(tmp.path());
        let np = empty_store(tmp.path());
        let id = load_or_adopt(&np).unwrap();
        let (_, pem) = x509_parser::pem::parse_x509_pem(id.cert_pem.as_bytes()).unwrap();
        let x509 = pem.parse_x509().unwrap();
        assert_eq!(
            x509.public_key().algorithm.algorithm.to_id_string(),
            "1.2.840.10045.2.1", // id-ecPublicKey
        );
        assert!(id.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(tmp.path().join("native-cert.pem").exists());
        assert!(tmp.path().join("native-key.pem").exists());
        let again = load_or_adopt(&np).unwrap();
        assert_eq!(again.cert_pem, id.cert_pem);
    }

    #[test]
    fn keeps_legacy_rsa_while_native_pairings_exist() {
        let _serial = super::CONFIG_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set(tmp.path());
        let np = empty_store(tmp.path());
        np.add("old-client", &"ab".repeat(32)).unwrap();
        // Opaque bytes on purpose: the fallback serves cert.pem/key.pem
        // verbatim (PEM-only; no `rsa` crate).
        std::fs::write(tmp.path().join("cert.pem"), "legacy cert pem").unwrap();
        std::fs::write(tmp.path().join("key.pem"), "legacy key pem").unwrap();
        let id = load_or_adopt(&np).unwrap();
        assert_eq!(id.cert_pem, "legacy cert pem");
        assert!(!tmp.path().join("native-cert.pem").exists());
    }

    #[test]
    fn mints_p256_when_legacy_files_vanished() {
        let _serial = super::CONFIG_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set(tmp.path());
        let np = empty_store(tmp.path());
        np.add("stranded-client", &"cd".repeat(32)).unwrap();
        let id = load_or_adopt(&np).unwrap();
        assert!(id.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(tmp.path().join("native-cert.pem").exists());
    }
}
