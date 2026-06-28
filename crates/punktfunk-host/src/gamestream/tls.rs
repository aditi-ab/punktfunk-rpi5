//! TLS for the HTTPS nvhttp port (47984) and the management API. Moonlight does **mutual TLS** —
//! it presents its client cert and expects the server to request one — so a plain server-auth
//! config makes the post-pairing `pairchallenge` fail. This config requests the client cert and
//! verifies the client owns its key, but accepts any well-formed cert at the *handshake* (the
//! pairing ceremony is the real proof of identity). Authorization against the paired allow-list is
//! then enforced per-request: [`serve_https`] reads the verified peer cert and attaches its
//! fingerprint ([`PeerCertFingerprint`]) to each request, and the nvhttp/mgmt handlers reject
//! callers whose fingerprint is not pinned (mirroring Apollo's post-handshake `get_verified_cert`).

use anyhow::{anyhow, Context, Result};
use axum::Router;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, UnixTime};
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
                tracing::warn!(error = %e, "HTTPS accept failed");
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
