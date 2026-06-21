//! The host's self-signed RSA-2048 identity: the cert returned to clients as `plaincert`
//! during pairing AND presented as the TLS server cert on 47984 (Moonlight pins it). The
//! cert's own X.509 signature bytes are an input to the pairing hashes, so we extract them.

use super::config_dir;
use anyhow::{anyhow, Context, Result};
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::RsaPrivateKey;
use sha2::Sha256;
use std::fs;

pub struct ServerIdentity {
    /// PEM of the cert (returned hex-encoded as `plaincert`; also the TLS server cert).
    pub cert_pem: String,
    /// PKCS#8 PEM of the private key (TLS server key).
    pub key_pem: String,
    /// The cert's X.509 `signatureValue` bytes — bound into the pairing challenge hashes.
    pub signature: Vec<u8>,
    /// RSA-PKCS1v15-SHA256 signer over the host key (the pairing `sign256`).
    pub signing_key: SigningKey<Sha256>,
}

impl ServerIdentity {
    pub fn load_or_create() -> Result<ServerIdentity> {
        let dir = config_dir();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        let (cert_pem, key_pem) = match (
            fs::read_to_string(&cert_path),
            fs::read_to_string(&key_path),
        ) {
            (Ok(c), Ok(k)) if !c.trim().is_empty() && !k.trim().is_empty() => (c, k),
            _ => {
                let (c, k) = generate()?;
                // The private key is the trust root for EVERY surface (TLS server cert, pairing
                // signing, the QUIC identity clients pin) — write it owner-only (0600 / SYSTEM-only
                // DACL) so a local user can't read it and impersonate the host. The dir is 0700.
                super::create_private_dir(&dir).ok();
                super::write_secret_file(&key_path, k.as_bytes())
                    .with_context(|| format!("write {}", key_path.display()))?;
                // The cert is public (handed to clients), but write it owner-only too for consistency.
                super::write_secret_file(&cert_path, c.as_bytes())
                    .with_context(|| format!("write {}", cert_path.display()))?;
                tracing::info!(path = %cert_path.display(), "generated punktfunk host certificate (RSA-2048, key 0600)");
                (c, k)
            }
        };
        Self::from_pems(cert_pem, key_pem)
    }

    /// Build an identity from PEMs (no I/O).
    pub fn from_pems(cert_pem: String, key_pem: String) -> Result<ServerIdentity> {
        let priv_key = RsaPrivateKey::from_pkcs8_pem(&key_pem).context("parse host private key")?;
        let signing_key = SigningKey::<Sha256>::new(priv_key);
        let signature = cert_signature(&cert_pem)?;
        Ok(ServerIdentity {
            cert_pem,
            key_pem,
            signature,
            signing_key,
        })
    }

    /// Throwaway in-memory identity — nothing touches the config dir (used by tests).
    pub fn ephemeral() -> Result<ServerIdentity> {
        let (cert_pem, key_pem) = generate()?;
        Self::from_pems(cert_pem, key_pem)
    }
}

fn generate() -> Result<(String, String)> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256).context("rcgen RSA keygen")?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).context("cert params")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "punktfunk");
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2040, 1, 1);
    let cert = params.self_signed(&key).context("self-sign cert")?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Extract the X.509 `signatureValue` bytes from a cert PEM.
fn cert_signature(cert_pem: &str) -> Result<Vec<u8>> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| anyhow!("parse cert pem: {e}"))?;
    let x509 = pem.parse_x509().context("parse x509")?;
    Ok(x509.signature_value.data.to_vec())
}
