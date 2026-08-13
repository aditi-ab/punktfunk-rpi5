//! The host's self-signed RSA-2048 identity: the cert returned to clients as `plaincert`
//! during pairing AND presented as the TLS server cert on 47984 (Moonlight pins it). The
//! cert's own X.509 signature bytes are an input to the pairing hashes, so we extract them.

use anyhow::{anyhow, Context, Result};
use pf_paths::config_dir;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::RsaPrivateKey;
// `rsa`'s own re-export: this `Sha256` is a TYPE PARAMETER to `SigningKey`, so it has to be the
// one `rsa 0.9`'s `digest 0.10` traits speak — not the crate-wide `sha2 0.11`. See Cargo.toml.
use rsa::sha2::Sha256;
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
        // Harden the directory BEFORE the first read, not only in the branch that generates a new
        // identity (2026-08-05 review M-1). Reading first is what made the hardening pointless
        // against the attack it was written for: combined with H-4's pre-creatable
        // `%ProgramData%\punktfunk`, a local user could plant a cert/key pair and have it adopted
        // verbatim as the host's long-lived identity — the QUIC server key, the mgmt-API TLS key and
        // the RSA pairing signer all becoming a key the attacker holds. The compromise is permanent:
        // this function never regenerates while both files are non-empty.
        pf_paths::create_private_dir(&dir).ok();
        let (cert_pem, key_pem) = match (
            fs::read_to_string(&cert_path),
            fs::read_to_string(&key_path),
        ) {
            (Ok(c), Ok(k)) if !c.trim().is_empty() && !k.trim().is_empty() => (c, k),
            _ => {
                let (c, k) = generate()?;
                // The private key is the trust root for EVERY surface (TLS server cert, pairing
                // signing, the QUIC identity clients pin) — write it owner-only (0600 / SYSTEM-only
                // DACL) so a local user can't read it and impersonate the host. The dir is already
                // 0700 / SYSTEM+Admins from the unconditional hardening above.
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
    // rcgen cannot *generate* an RSA key on either backend — `generate_for(&PKCS_RSA_SHA256)`
    // returns `KeyGenerationUnavailable`. Moonlight requires an RSA-2048 identity, so generate the
    // key with the pure-Rust `rsa` crate (already a dep for the pairing signer) and hand the PKCS#8
    // PEM to rcgen, which *can* load an existing RSA key and self-sign with it. Returning that same
    // PEM keeps it byte-identical to what `from_pems` re-parses.
    //
    // This path runs ONLY when no cert exists yet — a fresh install — so an upgraded box never
    // re-executes it.
    //
    // The rng comes from `rsa`'s OWN rand_core re-export, not from our `rand`. `RsaPrivateKey::new`
    // is bounded on rand_core **0.6**'s `CryptoRngCore`, and since the host moved to rand 0.9 its
    // `ThreadRng` implements rand_core 0.9's traits instead — a different trait of the same name,
    // so it no longer satisfies the bound. `rsa::rand_core::OsRng` is the OS CSPRNG under the
    // exact traits `rsa` compiled against, which keeps the two rand_core majors from meeting.
    let priv_key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
        .context("generate RSA-2048 host key")?;
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

/// Coverage for what the aws-lc-rs migration (#192) changed here but shipped unverified.
///
/// `generate()` was already reached by other tests via `ServerIdentity::ephemeral()`, but only ever
/// as an unasserted fixture — nothing checked that what came back was still an RSA-2048 identity,
/// which is the one property Moonlight requires. The handshake behaviour had no coverage at all,
/// and the GameStream TLS path is the single place where a legacy peer meets the new backend, so
/// a backend or feature regression there would surface first in the field.
#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
    use rustls::{
        ClientConfig, ClientConnection, DigitallySignedStruct, ServerConnection, SignatureScheme,
    };
    use std::sync::Arc;

    /// Moonlight does not chain-verify the host: it pins the cert by SHA-256 out of band, exactly
    /// as our own `AcceptAnyClientCert` does in the other direction. Modelling that is what makes
    /// this a Moonlight-shaped peer rather than a webpki-clean one.
    #[derive(Debug)]
    struct PinsOutOfBand(Arc<CryptoProvider>);

    impl ServerCertVerifier for PinsOutOfBand {
        fn verify_server_cert(
            &self,
            _e: &CertificateDer,
            _i: &[CertificateDer],
            _s: &ServerName,
            _o: &[u8],
            _n: UnixTime,
        ) -> std::result::Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &CertificateDer,
            d: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &CertificateDer,
            d: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    fn parts(
        cert_pem: &str,
        key_pem: &str,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let certs = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("cert pem")
            .into_iter()
            .map(|c| c.into_owned())
            .collect();
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
            .expect("key pem")
            .clone_key();
        (certs, key)
    }

    /// Shuttle handshake bytes between the two ends until both stop handshaking.
    fn pump(client: &mut ClientConnection, server: &mut ServerConnection) {
        for _ in 0..40 {
            while client.wants_write() {
                let mut buf = Vec::new();
                client.write_tls(&mut buf).expect("client write");
                let mut cur = &buf[..];
                while !cur.is_empty() {
                    if server.read_tls(&mut cur).expect("server read") == 0 {
                        break;
                    }
                    server.process_new_packets().expect("server handshake");
                }
            }
            while server.wants_write() {
                let mut buf = Vec::new();
                server.write_tls(&mut buf).expect("server write");
                let mut cur = &buf[..];
                while !cur.is_empty() {
                    if client.read_tls(&mut cur).expect("client read") == 0 {
                        break;
                    }
                    client.process_new_packets().expect("client handshake");
                }
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                return;
            }
        }
        panic!("handshake did not converge");
    }

    /// Run a mutual handshake against the real `tls::server_config` and return the negotiated
    /// (protocol version, key exchange group, number of client certs the server saw).
    fn handshake_against_host(
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> (String, String, usize) {
        let (host_cert, host_key) = generate().expect("host identity");
        // A Moonlight client's own identity is an RSA-2048 self-signed cert — the same shape.
        let (peer_cert, peer_key) = generate().expect("peer identity");

        let server_cfg =
            crate::gamestream::tls::server_config(&host_cert, &host_key).expect("server config");
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let (pc, pk) = parts(&peer_cert, &peer_key);
        let client_cfg = ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(versions)
            .expect("client versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinsOutOfBand(provider)))
            .with_client_auth_cert(pc, pk)
            .expect("client auth cert");

        let mut server = ServerConnection::new(server_cfg).expect("server conn");
        let mut client = ClientConnection::new(
            Arc::new(client_cfg),
            ServerName::try_from("punktfunk").unwrap(),
        )
        .expect("client conn");
        pump(&mut client, &mut server);

        let version = format!("{:?}", client.protocol_version().expect("version"));
        let kx = client
            .negotiated_key_exchange_group()
            .map(|g| format!("{:?}", g.name()))
            .unwrap_or_default();
        let seen = server.peer_certificates().map(|c| c.len()).unwrap_or(0);
        (version, kx, seen)
    }

    /// The fresh-install path. rcgen cannot generate an RSA key, so this leans on `rsa` for the key
    /// and rcgen only to self-sign — a split that must keep working across a crypto-backend change.
    #[test]
    fn generate_mints_a_loadable_rsa2048_identity() {
        let (cert_pem, key_pem) = generate().expect("generate");
        assert!(cert_pem.contains("BEGIN CERTIFICATE"), "cert is not PEM");
        assert!(
            key_pem.contains("BEGIN PRIVATE KEY"),
            "key is not PKCS#8 PEM"
        );

        // Everything downstream (pairing hashes, the TLS server cert) goes through from_pems.
        let identity = ServerIdentity::from_pems(cert_pem, key_pem).expect("from_pems");
        // An RSA-2048 signature is exactly 256 bytes; this pins the key size that Moonlight needs
        // without depending on an `rsa` accessor that could change shape.
        assert_eq!(
            identity.signature.len(),
            256,
            "host cert signature should be RSA-2048 (256 bytes)"
        );
    }

    /// GAP 1 from the #192 handoff: a legacy peer negotiating TLS 1.2 against the new backend.
    #[test]
    fn moonlight_shaped_peer_completes_a_tls12_mutual_handshake() {
        let (version, _kx, client_certs_seen) = handshake_against_host(&[&rustls::version::TLS12]);
        assert_eq!(version, "TLSv1_2", "Moonlight negotiates TLS 1.2");
        assert_eq!(
            client_certs_seen, 1,
            "mutual TLS: the host must receive the peer's client cert"
        );
    }

    /// GAP 3 from the #192 handoff: `prefer-post-quantum` was asserted from the rustls source and
    /// never observed. Pin it, so a provider or feature regression that silently drops ML-KEM back
    /// to a classical group fails here instead of in the field.
    #[test]
    fn tls13_negotiates_the_post_quantum_group() {
        let (version, kx, client_certs_seen) = handshake_against_host(&[&rustls::version::TLS13]);
        assert_eq!(version, "TLSv1_3");
        assert_eq!(
            kx, "X25519MLKEM768",
            "post-quantum key exchange must be preferred"
        );
        assert_eq!(client_certs_seen, 1);
    }
}
