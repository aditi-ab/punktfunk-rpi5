//! TLS for the HTTPS nvhttp port (47984). Moonlight does **mutual TLS** — it presents its
//! client cert and expects the server to request one — so a plain server-auth config makes
//! the post-pairing `pairchallenge` fail. This config requests the client cert and verifies
//! the client owns its key, but (for now) accepts any well-formed cert; enforcing the
//! paired allow-list (rejecting unpaired clients on /launch) is a follow-up hardening step.

use anyhow::{anyhow, Context, Result};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use std::sync::Arc;

/// Requests + signature-checks the client cert but accepts any (the pairing handshake is
/// the real proof). Pinning to the paired set is a hardening follow-up.
#[derive(Debug)]
struct AcceptAnyClientCert {
    provider: Arc<CryptoProvider>,
    /// nvhttp/pairing REQUIRES the client cert (mandatory); the mgmt API REQUESTS it but lets a
    /// certless peer (a browser, falling back to a bearer token) through (optional).
    mandatory: bool,
}

impl ClientCertVerifier for AcceptAnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        self.mandatory
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer,
        _intermediates: &[CertificateDer],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build a mutual-TLS `ServerConfig` presenting the host cert/key. nvhttp/pairing path: the
/// client cert is **mandatory**.
pub fn server_config(cert_pem: &str, key_pem: &str) -> Result<Arc<ServerConfig>> {
    build_server_config(cert_pem, key_pem, true)
}

/// Like [`server_config`] but the client cert is **optional** — a certless peer (a browser using a
/// bearer token) still completes the handshake. Used by the management API's mTLS auth: a paired
/// client presents its cert (authorized by fingerprint), everyone else falls back to the token.
pub fn server_config_optional_client(cert_pem: &str, key_pem: &str) -> Result<Arc<ServerConfig>> {
    build_server_config(cert_pem, key_pem, false)
}

fn build_server_config(
    cert_pem: &str,
    key_pem: &str,
    mandatory: bool,
) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let certs = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse host cert PEM")?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .context("parse host key PEM")?
        .ok_or_else(|| anyhow!("no private key in host key PEM"))?;

    let verifier = Arc::new(AcceptAnyClientCert {
        provider: provider.clone(),
        mandatory,
    });
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls protocol versions")?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .context("rustls server cert")?;
    Ok(Arc::new(config))
}
