//! Shared TLS trust primitives for the punktfunk clients: the certificate-fingerprint hash and
//! the one canonical fingerprint-pinning [`ServerCertVerifier`](rustls::client::danger::ServerCertVerifier)
//! (`PinVerify`). Trust across the whole system is the SHA-256 of the host's self-signed leaf cert
//! (TOFU-pinned), not a CA chain — and this verifier is what the QUIC connect, the game-library
//! HTTP client, and the tray status poll all share, instead of hand-rolling it three times on a
//! trust boundary. Behind the light `tls` feature (rustls + sha2 only — no QUIC runtime), which
//! the heavier `quic` feature pulls in.

use std::sync::{Arc, Mutex};

/// SHA-256 of a certificate's DER encoding — the fingerprint clients pin. Re-exported as
/// `crate::quic::endpoint::cert_fingerprint` for callers that already reach it there.
pub fn cert_fingerprint(cert_der: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(cert_der).into()
}

/// Fingerprint-pinning verifier: trust is the SHA-256 of the host's (self-signed) leaf cert,
/// not a CA chain.
///
/// - `pin = Some(sha256)` rejects any host whose leaf doesn't hash to `sha256`.
/// - `pin = None` accepts any leaf (trust-on-first-use) — pair with [`with_observed`](Self::with_observed)
///   to record what was seen so the embedder can persist it and pin it from then on.
///
/// The handshake signatures are ALWAYS verified for real even though the cert is pinned:
/// `CertificateVerify` is what proves the peer *holds the pinned cert's private key* — skip it and
/// an active MITM can replay the host's (public) certificate, match the pin, and complete the
/// handshake with its own key.
#[derive(Debug)]
pub struct PinVerify {
    pin: Option<[u8; 32]>,
    observed: Option<Arc<Mutex<Option<[u8; 32]>>>>,
}

impl PinVerify {
    /// A verifier that pins `pin` (or accepts any when `None`) without recording what it saw —
    /// the HTTP clients, which connect with a known pin or accept-any and never need to persist
    /// the observed fingerprint.
    pub fn new(pin: Option<[u8; 32]>) -> Self {
        Self {
            pin,
            observed: None,
        }
    }

    /// Like [`new`](Self::new) but also writes the observed leaf fingerprint into `slot` during
    /// the handshake, so a trust-on-first-use caller (the QUIC connect) can read it off the slot
    /// and pin it on the next connect.
    pub fn with_observed(pin: Option<[u8; 32]>, slot: Arc<Mutex<Option<[u8; 32]>>>) -> Self {
        Self {
            pin,
            observed: Some(slot),
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for PinVerify {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // Only hash the leaf when the result depends on it: a pin to check and/or a slot to
        // record into. Accept-any-without-recording (an un-pinned HTTP agent) skips it.
        if self.pin.is_some() || self.observed.is_some() {
            let fp = cert_fingerprint(end_entity.as_ref());
            if let Some(slot) = &self.observed {
                *slot.lock().unwrap() = Some(fp);
            }
            if let Some(expected) = self.pin {
                if fp != expected {
                    return Err(rustls::Error::InvalidCertificate(
                        rustls::CertificateError::ApplicationVerificationFailure,
                    ));
                }
            }
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
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
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    /// Drive the pin check against `cert_bytes`. `verify_server_cert` only hashes the leaf, so
    /// arbitrary bytes stand in for a DER certificate here.
    fn verify(v: &PinVerify, cert_bytes: &[u8]) -> std::result::Result<(), rustls::Error> {
        let der = CertificateDer::from(cert_bytes.to_vec());
        let name = ServerName::try_from("punktfunk").unwrap();
        v.verify_server_cert(
            &der,
            &[],
            &name,
            &[],
            UnixTime::since_unix_epoch(std::time::Duration::ZERO),
        )
        .map(|_| ())
    }

    #[test]
    fn matching_pin_accepts_and_mismatch_is_rejected() {
        let cert = b"the-host-leaf-cert";
        let good = cert_fingerprint(cert);
        assert!(verify(&PinVerify::new(Some(good)), cert).is_ok());

        let mut wrong = good;
        wrong[0] ^= 0xff;
        match verify(&PinVerify::new(Some(wrong)), cert) {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )) => {}
            other => panic!("a pin mismatch must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn no_pin_accepts_any_and_records_the_observed_fingerprint() {
        let cert = b"whatever-the-host-presents";
        let slot = Arc::new(Mutex::new(None));
        let v = PinVerify::with_observed(None, slot.clone());
        assert!(verify(&v, cert).is_ok());
        assert_eq!(*slot.lock().unwrap(), Some(cert_fingerprint(cert)));
    }
}
