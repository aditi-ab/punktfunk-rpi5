//! TLS for the HTTPS nvhttp port (47984) and the management API. Moonlight does **mutual TLS** —
//! it presents its client cert and expects the server to request one — so a plain server-auth
//! config makes the post-pairing `pairchallenge` fail. This config requests the client cert and
//! verifies the client owns its key, but accepts any well-formed cert at the *handshake* (the
//! pairing ceremony is the real proof of identity). Authorization against the paired allow-list is
//! then enforced per-request: [`serve_https`] reads the verified peer cert and attaches its
//! fingerprint ([`PeerCertFingerprint`]) to each request, and the nvhttp/mgmt handlers reject
//! callers whose fingerprint is not pinned (mirroring Apollo's post-handshake `get_verified_cert`).

use anyhow::{Context, Result};
use axum::Router;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;

/// SHA-256 of the peer's client certificate (hex), injected per-connection into each request's
/// extensions by [`serve_https`]; `None` when the peer presented no client cert (plain HTTP, or a
/// browser falling back to a bearer token). Handlers authorize a request whose fingerprint is in
/// the paired store.
#[derive(Clone)]
pub(crate) struct PeerCertFingerprint(pub Option<String>);

/// The TCP source address of an HTTPS request, injected per-connection by [`serve_https`]. Used by
/// `/launch` to record which paired client owns the session so the unauthenticated RTSP/UDP media
/// plane can bind to that peer's IP (security-review 2026-06-28 #4).
#[derive(Clone, Copy)]
pub(crate) struct PeerAddr(pub SocketAddr);

/// HTTPS server that surfaces the verified client cert to handlers. `axum_server` can't expose the
/// peer cert, so this runs the rustls handshake itself (tokio-rustls), reads the peer certificate,
/// and serves the axum `Router` over hyper with the peer's fingerprint attached to every request as
/// a [`PeerCertFingerprint`] extension. Shared by the nvhttp HTTPS listener and the management API.
pub(crate) async fn serve_https(
    bind: SocketAddr,
    app: Router,
    tls: Arc<ServerConfig>,
) -> Result<()> {
    use tower::ServiceExt;
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind HTTPS {bind}"))?;
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                // A persistent accept() error (fd exhaustion / EMFILE) would otherwise hot-spin
                // this loop and storm the log; back off so a stuck accept can't burn a core.
                tracing::warn!(error = %e, "HTTPS accept failed — backing off 100ms");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                // A failed handshake is routine (port scan, a browser bailing on the self-signed
                // cert, a peer that hung up) — not fatal.
                Err(_) => return,
            };
            // The verified peer cert (the verifier accepts any well-formed one; handlers authorize
            // by fingerprint) → its SHA-256, matched against the paired store.
            let fp = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .map(|c| hex::encode(punktfunk_core::quic::endpoint::cert_fingerprint(c.as_ref())));
            let fp = PeerCertFingerprint(fp);
            let addr = PeerAddr(peer);
            let svc =
                hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let app = app.clone();
                    let fp = fp.clone();
                    async move {
                        let mut req = req.map(axum::body::Body::new);
                        req.extensions_mut().insert(fp);
                        req.extensions_mut().insert(addr);
                        app.oneshot(req).await // Router error is Infallible
                    }
                });
            let io = hyper_util::rt::TokioIo::new(tls_stream);
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(io, svc)
                    .await;
        });
    }
}

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
        let verdict = verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        );
        // The Moonlight-client-cert leniency exists only when the compat planes do (WP19) —
        // native clients present webpki-clean certs and never need it.
        #[cfg(feature = "gamestream")]
        let verdict = verdict.or_else(|e| accept_legacy_moonlight_cert(message, cert, dss, e));
        verdict
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let verdict = verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        );
        // The Moonlight-client-cert leniency exists only when the compat planes do (WP19) —
        // native clients present webpki-clean certs and never need it.
        #[cfg(feature = "gamestream")]
        let verdict = verdict.or_else(|e| accept_legacy_moonlight_cert(message, cert, dss, e));
        verdict
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Fallback client-cert `CertificateVerify` check that tolerates pre-v3 (X.509 v2) certificates,
/// invoked only when rustls-webpki's strict path has already rejected the cert.
///
/// **The compatibility gap.** rustls-webpki (0.103) accepts *only* X.509 v3 certs and returns
/// `UnsupportedCertVersion` for anything older — aborting the mTLS handshake before it ever looks at
/// the signature. The moonlight-embedded client family (moonlight-embedded, aurora-tv, …) still mint
/// self-signed **v2** certs with no `keyUsage` extension, so pairing fails against us while it works
/// against Sunshine, whose OpenSSL verify callback never inspects the cert version. The decisive
/// factor is the cert *version*, not the missing `keyUsage` — a v3 cert with no extensions passes
/// webpki fine.
///
/// **Why relaxing it does not weaken security.** The cert's X.509 version and extensions carry no
/// security weight in GameStream: the client cert is self-signed and later pinned by SHA-256
/// (`nvhttp::peer_is_paired`), and the PIN pairing ceremony is the real proof of identity. The one
/// property that *does* matter — and that we still enforce here — is that the peer holds the private
/// key for the cert it presented (the TLS `CertificateVerify` signature), because the cert itself is
/// exchanged in the clear over the plain-HTTP pairing port and is not a secret. So we re-run
/// *exactly that* check with a version-agnostic parser: pull the RSA public key from the lenient
/// x509-parser and verify the handshake signature ourselves (the same primitive
/// `pairing::verify256` uses). This is a full cryptographic verification, never a bypass — a bad
/// signature still fails, and any non-RSA / unsupported scheme falls through to webpki's original
/// error `webpki_err`. Moonlight/Sunshine client certs are RSA-2048, so this matches Sunshine's
/// leniency without loosening the pinned trust model.
#[cfg(feature = "gamestream")]
fn accept_legacy_moonlight_cert(
    message: &[u8],
    cert: &CertificateDer,
    dss: &DigitallySignedStruct,
    webpki_err: rustls::Error,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    use rsa::pkcs8::DecodePublicKey;
    use rsa::signature::Verifier;
    use rsa::{pkcs1v15, pss, RsaPublicKey};
    // `rsa`'s own re-export — these are `pkcs1v15`/`pss` type parameters on `rsa 0.9`
    // (`digest 0.10`), not the crate-wide `sha2 0.11`. See Cargo.toml.
    use rsa::sha2::{Sha256, Sha384, Sha512};

    let Ok((_, x509)) = x509_parser::parse_x509_certificate(cert.as_ref()) else {
        return Err(webpki_err);
    };
    let Ok(key) = RsaPublicKey::from_public_key_der(x509.public_key().raw) else {
        return Err(webpki_err); // not an RSA cert — leave webpki's verdict in place
    };
    let sig = dss.signature();

    // `VerifyingKey`/`Signature` hash `message` internally with the scheme's digest; each arm moves
    // `key`, but exactly one arm runs. Covers the RSA PKCS#1v1.5 and RSA-PSS schemes a Moonlight
    // client offers in `CertificateVerify` (SHA-256/384/512).
    let ok = match dss.scheme {
        SignatureScheme::RSA_PKCS1_SHA256 => pkcs1v15::Signature::try_from(sig)
            .map(|s| {
                pkcs1v15::VerifyingKey::<Sha256>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        SignatureScheme::RSA_PKCS1_SHA384 => pkcs1v15::Signature::try_from(sig)
            .map(|s| {
                pkcs1v15::VerifyingKey::<Sha384>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        SignatureScheme::RSA_PKCS1_SHA512 => pkcs1v15::Signature::try_from(sig)
            .map(|s| {
                pkcs1v15::VerifyingKey::<Sha512>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        SignatureScheme::RSA_PSS_SHA256 => pss::Signature::try_from(sig)
            .map(|s| {
                pss::VerifyingKey::<Sha256>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        SignatureScheme::RSA_PSS_SHA384 => pss::Signature::try_from(sig)
            .map(|s| {
                pss::VerifyingKey::<Sha384>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        SignatureScheme::RSA_PSS_SHA512 => pss::Signature::try_from(sig)
            .map(|s| {
                pss::VerifyingKey::<Sha512>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        _ => return Err(webpki_err),
    };

    if ok {
        tracing::debug!(
            "accepted a legacy (pre-v3) Moonlight client cert via version-agnostic RSA verify"
        );
        Ok(HandshakeSignatureValid::assertion())
    } else {
        Err(webpki_err)
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
    // PEM parsing via rustls-pki-types (the same `PemObject` path punktfunk-core/quic.rs uses),
    // so we don't pull the unmaintained `rustls-pemfile`.
    let certs = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse host cert PEM")?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).context("parse host key PEM")?;

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
