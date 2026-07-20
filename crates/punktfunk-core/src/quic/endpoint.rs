//! Shared QUIC endpoint construction (host + client) and transport tuning — keep-alive and idle
//! timeout, plus the certificate-fingerprint helpers. The cert-pinning verifier itself is the
//! one shared [`crate::tls::PinVerify`]; this module only wires it into the client endpoint.
use std::sync::{Arc, Mutex};

/// Shared QUIC transport tuning for BOTH the host and client endpoints. Keep-alive is the
/// load-bearing setting: with quinn's defaults it is OFF, so any quiet stretch on the
/// connection (no input, audio muted or stalled, a capture hiccup, a mode change) lets the
/// idle timer run out and quinn closes the session — surfacing to the embedder as
/// `next_au` → Closed. The native equivalent of Moonlight's ENet keepalive: a small PING
/// every `KEEP_ALIVE` keeps the path warm. The interval sits well under `MAX_IDLE` so
/// several keepalives can be lost back-to-back (a wifi roam, a brief blip) without a false
/// close, while a genuinely dead peer is still detected within `MAX_IDLE`.
/// The default control-connection idle timeout (disconnect-detection latency). A vanished client
/// is declared dead within this window — the Windows IDD-push path needs it short so a RECONNECT
/// recreates a fresh virtual monitor instead of joining the still-lingering old session; the Linux
/// path pairs it with the same-client reconnect preempt. Host-tunable via `server_with_identity_idle`.
pub const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

fn stream_transport() -> Arc<quinn::TransportConfig> {
    stream_transport_idle(DEFAULT_IDLE_TIMEOUT)
}

/// Transport config with a caller-chosen idle timeout (disconnect-detection latency). The
/// keep-alive interval tracks it at half the idle window (capped at the default 4s), so a live
/// path is PINGed at least twice per window and a single lost PING (wifi roam / brief blip) won't
/// false-close. `idle` is clamped to a ≥1s floor so a misconfigured tiny value can't tear live
/// sessions down. Active sessions are unaffected either way: video keeps the connection live and
/// the keep-alive holds it open through quiet control periods.
fn stream_transport_idle(idle: std::time::Duration) -> Arc<quinn::TransportConfig> {
    use std::time::Duration;
    let idle = idle.max(Duration::from_secs(1));
    let keep_alive = (idle / 2).min(Duration::from_secs(4));
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(idle).expect("clamped idle timeout is a valid QUIC value"),
    ));
    t.keep_alive_interval(Some(keep_alive));
    // The datagram planes (audio/rumble/hidout/host-timing host→client; mic/rich-input
    // client→host) carry realtime state, not bulk data — but they are congestion-controlled,
    // unlike video, which rides its own latest-wins UDP path. quinn's default 1 MiB datagram
    // send buffer is a FIFO that only sheds oldest-first at the cap, so on a congested link
    // (Wi-Fi under streaming load) it holds tens of seconds of Opus: audio and rumble build a
    // standing delay that never drains while video stays live. Capping the buffer makes the
    // plane latest-wins at the source — ~200 ms of stereo Opus (proportionally less at
    // surround bitrates), so sustained congestion costs concealable drops, never lag.
    t.datagram_send_buffer_size(4 * 1024);
    Arc::new(t)
}

/// Server endpoint with a fresh self-signed certificate (tests/dev — production hosts
/// persist an identity and use [`server_with_identity`] so clients can pin it).
pub fn server(addr: std::net::SocketAddr) -> anyhow_result::Result<quinn::Endpoint> {
    let cert = rcgen::generate_simple_self_signed(vec!["punktfunk".into()])
        .map_err(|e| anyhow_result::Error::msg(format!("self-signed cert: {e}")))?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    server_from_der(cert_der, key_der.into(), addr, DEFAULT_IDLE_TIMEOUT)
}

/// Server endpoint from a persisted PEM identity (certificate + PKCS#8 private key) —
/// the host's long-lived self-signed cert, so the fingerprint clients pin is stable
/// across restarts. Uses the [`DEFAULT_IDLE_TIMEOUT`]; see [`server_with_identity_idle`] to tune it.
pub fn server_with_identity(
    addr: std::net::SocketAddr,
    cert_pem: &str,
    key_pem: &str,
) -> anyhow_result::Result<quinn::Endpoint> {
    server_with_identity_idle(addr, cert_pem, key_pem, DEFAULT_IDLE_TIMEOUT)
}

/// Like [`server_with_identity`] but with a host-chosen control-connection idle timeout — the
/// disconnect-detection latency (how long a vanished client takes to be declared dead). Shorter =
/// faster teardown/linger of a dropped session; the value is clamped to a ≥1s floor and its
/// keep-alive scales with it so a live session never false-closes.
pub fn server_with_identity_idle(
    addr: std::net::SocketAddr,
    cert_pem: &str,
    key_pem: &str,
    idle: std::time::Duration,
) -> anyhow_result::Result<quinn::Endpoint> {
    use rustls::pki_types::pem::PemObject;
    let cert_der = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
        .map_err(|e| anyhow_result::Error::msg(format!("cert pem: {e}")))?;
    let key_der = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| anyhow_result::Error::msg(format!("key pem: {e}")))?;
    server_from_der(cert_der, key_der, addr, idle)
}

/// Fixed ALPN for the punktfunk/1 QUIC handshake. Pinning it rejects a cross-protocol peer at the
/// TLS layer (defense-in-depth) and makes the wire protocol explicit. Both ends set the SAME value;
/// a host with ALPN configured rejects a client that offers none, so client + host must be updated
/// together (acceptable while the protocol/ABI is still evolving).
const QUIC_ALPN: &[u8] = b"pkf1";

fn server_from_der(
    cert_der: rustls::pki_types::CertificateDer<'static>,
    key_der: rustls::pki_types::PrivateKeyDer<'static>,
    addr: std::net::SocketAddr,
    idle: std::time::Duration,
) -> anyhow_result::Result<quinn::Endpoint> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Client auth is OFFERED but optional: a client that presents its self-signed
    // identity is fingerprinted post-handshake (pairing / --require-pairing checks);
    // one that presents none still connects (and is rejected at the app layer when
    // pairing is required).
    let mut rustls_cfg = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCert))
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| anyhow_result::Error::msg(format!("server config: {e}")))?;
    rustls_cfg.alpn_protocols = vec![QUIC_ALPN.to_vec()];
    let quic_cfg = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)
        .map_err(|e| anyhow_result::Error::msg(format!("quic server config: {e}")))?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_cfg));
    server_config.transport_config(stream_transport_idle(idle)); // keep-alive — see stream_transport_idle
    Ok(quinn::Endpoint::server(server_config, addr)?)
}

/// Generate a fresh self-signed identity (certificate + PKCS#8 key, both PEM) — what a
/// client persists once and presents on every connect so hosts can recognize it.
pub fn generate_identity() -> anyhow_result::Result<(String, String)> {
    let cert = rcgen::generate_simple_self_signed(vec!["punktfunk-client".into()])
        .map_err(|e| anyhow_result::Error::msg(format!("self-signed cert: {e}")))?;
    Ok((cert.cert.pem(), cert.key_pair.serialize_pem()))
}

/// Fingerprint of the client certificate a connection presented (host side), if any.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<[u8; 32]> {
    let identity = conn.peer_identity()?;
    let certs = identity
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .ok()?;
    certs.first().map(|c| cert_fingerprint(c.as_ref()))
}

/// SHA-256 of a certificate's DER encoding — the fingerprint clients pin. The implementation is
/// [`crate::tls::cert_fingerprint`] (shared with the HTTP clients); re-exported here so callers
/// reaching it as `quic::endpoint::cert_fingerprint` keep working.
pub use crate::tls::cert_fingerprint;

/// Fingerprint of a PEM-encoded certificate (what a host logs/shows for pairing UX —
/// must match what the client's verifier computes from the DER on the wire).
pub fn fingerprint_of_pem(cert_pem: &str) -> anyhow_result::Result<[u8; 32]> {
    use rustls::pki_types::pem::PemObject;
    let der = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
        .map_err(|e| anyhow_result::Error::msg(format!("cert pem: {e}")))?;
    Ok(cert_fingerprint(der.as_ref()))
}

/// Client endpoint that skips certificate verification (TOFU bootstrap — read the
/// observed fingerprint off the slot and pin it on the next connect).
pub fn client_insecure() -> anyhow_result::Result<quinn::Endpoint> {
    client_pinned(None).0
}

/// What [`client_pinned`] returns: the endpoint plus the slot the verifier writes the
/// observed host fingerprint into during the handshake.
pub type PinnedClient = (
    anyhow_result::Result<quinn::Endpoint>,
    Arc<Mutex<Option<[u8; 32]>>>,
);

/// Client endpoint that verifies the host by certificate fingerprint.
///
/// `pin = Some(sha256)` rejects any host whose leaf cert doesn't hash to `sha256`;
/// `None` accepts any (trust-on-first-use). Either way the observed fingerprint is
/// written to the returned slot during the handshake, so a TOFU caller can persist it.
pub fn client_pinned(pin: Option<[u8; 32]>) -> PinnedClient {
    client_pinned_with_identity(pin, None)
}

/// [`client_pinned`], additionally presenting a client identity (PEM cert + PKCS#8
/// key) via TLS client auth — how a paired client identifies itself to the host.
pub fn client_pinned_with_identity(
    pin: Option<[u8; 32]>,
    identity: Option<(&str, &str)>,
) -> PinnedClient {
    let observed = Arc::new(Mutex::new(None));
    let ep = (|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let builder = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::tls::PinVerify::with_observed(
                pin,
                observed.clone(),
            )));
        let mut rustls_cfg = match identity {
            None => builder.with_no_client_auth(),
            Some((cert_pem, key_pem)) => {
                use rustls::pki_types::pem::PemObject;
                let cert =
                    rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
                        .map_err(|e| anyhow_result::Error::msg(format!("client cert pem: {e}")))?;
                let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
                    .map_err(|e| anyhow_result::Error::msg(format!("client key pem: {e}")))?;
                builder
                    .with_client_auth_cert(vec![cert], key)
                    .map_err(|e| anyhow_result::Error::msg(format!("client auth: {e}")))?
            }
        };
        // Must match the server's ALPN ([`QUIC_ALPN`]) or the handshake is rejected.
        rustls_cfg.alpn_protocols = vec![QUIC_ALPN.to_vec()];
        let quic_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)
            .map_err(|e| anyhow_result::Error::msg(format!("quic client config: {e}")))?;
        let mut client_cfg = quinn::ClientConfig::new(Arc::new(quic_cfg));
        client_cfg.transport_config(stream_transport()); // keep-alive — see stream_transport
        let mut ep = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())?;
        ep.set_default_client_config(client_cfg);
        Ok(ep)
    })();
    (ep, observed)
}

/// Minimal error plumbing without pulling anyhow into punktfunk-core's public API.
pub mod anyhow_result {
    pub type Result<T> = std::result::Result<T, Error>;
    #[derive(Debug)]
    pub struct Error(String);
    impl Error {
        pub fn msg(s: String) -> Self {
            Error(s)
        }
    }
    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Error {}
    impl From<std::io::Error> for Error {
        fn from(e: std::io::Error) -> Self {
            Error(e.to_string())
        }
    }
}

/// Server-side client-cert verifier: accept any (self-signed) client certificate but
/// verify the handshake signature for real — possession of the presented cert's key is
/// what makes the post-handshake fingerprint ([`peer_fingerprint`]) meaningful.
/// Authorization (is this fingerprint paired?) happens at the application layer.
#[derive(Debug)]
struct AcceptAnyClientCert;

impl rustls::server::danger::ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn client_auth_mandatory(&self) -> bool {
        false // unpaired/legacy clients still connect; gating is per-feature
    }

    fn verify_client_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use crate::quic::endpoint;

    #[test]
    fn fingerprint_is_sha256_of_der() {
        // Stable across calls, distinct for distinct certs.
        let a = endpoint::cert_fingerprint(b"cert-a");
        assert_eq!(a, endpoint::cert_fingerprint(b"cert-a"));
        assert_ne!(a, endpoint::cert_fingerprint(b"cert-b"));
    }
}
