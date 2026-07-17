//! The host's self-signed RSA-2048 identity: the cert returned to clients as `plaincert`
//! during pairing AND presented as the TLS server cert on 47984 (Moonlight pins it). The
//! cert's own X.509 signature bytes are an input to the pairing hashes, so we extract them.

use anyhow::{anyhow, Context, Result};
use pf_paths::config_dir;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
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
                pf_paths::create_private_dir(&dir).ok();
                pf_paths::write_secret_file(&key_path, k.as_bytes())
                    .with_context(|| format!("write {}", key_path.display()))?;
                // The cert is public (handed to clients), but write it owner-only too for consistency.
                pf_paths::write_secret_file(&cert_path, c.as_bytes())
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
    // The workspace is ring-only (aws-lc-sys breaks Windows CI — see the rustls/rcgen pins), and
    // `ring` can *sign* with an existing RSA key but cannot *generate* one: rcgen's ring backend
    // returns `KeyGenerationUnavailable` for `generate_for(&PKCS_RSA_SHA256)`. Moonlight requires an
    // RSA-2048 identity, so generate the key with the pure-Rust `rsa` crate (already a dep for the
    // pairing signer) and hand the PKCS#8 PEM to rcgen, whose ring backend *can* load + self-sign
    // with it. Returning that same PEM keeps it byte-identical to what `from_pems` re-parses.
    let mut rng = rand::thread_rng();
    let priv_key = RsaPrivateKey::new(&mut rng, 2048).context("generate RSA-2048 host key")?;
    let key_pem = priv_key
        .to_pkcs8_pem(LineEnding::LF)
        .context("encode host key as PKCS#8 PEM")?
        .to_string();
    let key = rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(&key_pem, &rcgen::PKCS_RSA_SHA256)
        .context("load RSA host key into rcgen")?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).context("cert params")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "punktfunk");
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2040, 1, 1);
    let cert = params.self_signed(&key).context("self-sign cert")?;
    Ok((cert.pem(), key_pem))
}

/// Extract the X.509 `signatureValue` bytes from a cert PEM.
fn cert_signature(cert_pem: &str) -> Result<Vec<u8>> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| anyhow!("parse cert pem: {e}"))?;
    let x509 = pem.parse_x509().context("parse x509")?;
    Ok(x509.signature_value.data.to_vec())
}
