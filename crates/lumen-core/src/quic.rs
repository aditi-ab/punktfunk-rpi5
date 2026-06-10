//! `lumen/1` — the native control plane (M3), gated behind the `quic` feature.
//!
//! GameStream is lumen's compatibility layer; this is the start of its own protocol. A QUIC
//! connection (quinn, tokio — control plane only, never the per-frame path) carries a
//! length-prefixed binary handshake on one bidirectional stream:
//!
//! ```text
//!   client → host  Hello   { abi_version }
//!   host → client  Welcome { abi_version, session: full data-plane Config + mode + UDP port }
//!   client → host  Start   { client_udp_port }
//! ```
//!
//! after which both sides bring up a [`crate::session::Session`] over a plain
//! [`UdpTransport`](crate::transport::udp) (native threads, no async) and the host streams.
//! The Welcome carries everything the M1 core negotiates — FEC scheme (including GF(2¹⁶)
//! Leopard, which GameStream can't express), shard sizing, crypto key/salt — so the data
//! plane is exactly the hardened M1 `Session`.
//!
//! Seed-stage transport security: the host presents a self-signed certificate and the client
//! accepts any (pairing/pinning lands with the trust model; the data plane's AES-GCM is
//! already real). All integers little-endian; every message is `u16 length || payload`.

use crate::config::{Config, FecConfig, FecScheme, Mode, ProtocolPhase, Role};
use crate::error::{LumenError, Result};

/// Protocol magic + version, first bytes of every message payload.
pub const MAGIC: &[u8; 4] = b"LMN1";

/// `client → host`: open the session, requesting a display mode (the host creates its
/// virtual output at exactly this size/refresh — native resolution end to end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub abi_version: u32,
    pub mode: Mode,
}

/// `host → client`: the complete session offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub abi_version: u32,
    /// Host UDP port for the data plane.
    pub udp_port: u16,
    pub mode: Mode,
    pub fec: FecConfig,
    pub shard_payload: u16,
    pub encrypt: bool,
    pub key: [u8; 16],
    pub salt: [u8; 4],
    /// Seed/testing: how many frames the host will send (0 = unbounded).
    pub frames: u32,
}

/// `client → host`: data plane is bound, begin streaming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    pub client_udp_port: u16,
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(20);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.abi_version.to_le_bytes());
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Hello> {
        if b.len() < 20 || &b[0..4] != MAGIC {
            return Err(LumenError::InvalidArg("bad Hello"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Ok(Hello {
            abi_version: u32at(4),
            mode: Mode {
                width: u32at(8),
                height: u32at(12),
                refresh_hz: u32at(16),
            },
        })
    }
}

impl Welcome {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.abi_version.to_le_bytes());
        b.extend_from_slice(&self.udp_port.to_le_bytes());
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b.push(match self.fec.scheme {
            FecScheme::Gf8 => 0,
            FecScheme::Gf16 => 1,
        });
        b.push(self.fec.fec_percent);
        b.extend_from_slice(&self.fec.max_data_per_block.to_le_bytes());
        b.extend_from_slice(&self.shard_payload.to_le_bytes());
        b.push(self.encrypt as u8);
        b.extend_from_slice(&self.key);
        b.extend_from_slice(&self.salt);
        b.extend_from_slice(&self.frames.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Welcome> {
        // Layout (LE): magic[0..4] abi[4..8] port[8..10] w[10..14] h[14..18] hz[18..22]
        // scheme[22] pct[23] max_data[24..26] shard[26..28] encrypt[28] key[29..45]
        // salt[45..49] frames[49..53].
        if b.len() < 53 || &b[0..4] != MAGIC {
            return Err(LumenError::InvalidArg("bad Welcome"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let mut key = [0u8; 16];
        key.copy_from_slice(&b[29..45]);
        let mut salt = [0u8; 4];
        salt.copy_from_slice(&b[45..49]);
        Ok(Welcome {
            abi_version: u32at(4),
            udp_port: u16at(8),
            mode: Mode {
                width: u32at(10),
                height: u32at(14),
                refresh_hz: u32at(18),
            },
            fec: FecConfig {
                scheme: if b[22] == 1 {
                    FecScheme::Gf16
                } else {
                    FecScheme::Gf8
                },
                fec_percent: b[23],
                max_data_per_block: u16at(24),
            },
            shard_payload: u16at(26),
            encrypt: b[28] != 0,
            key,
            salt,
            frames: u32at(49),
        })
    }

    /// Build the data-plane [`Config`] this offer describes (for `role`).
    pub fn session_config(&self, role: Role) -> Config {
        let mut c = Config::p1_defaults(role);
        c.phase = ProtocolPhase::P1GameStream; // wire phase id pending the P2 packet rev
        c.fec = self.fec;
        c.shard_payload = self.shard_payload as usize;
        c.encrypt = self.encrypt;
        c.key = self.key;
        c.salt = self.salt;
        c
    }
}

impl Start {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(6);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.client_udp_port.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Start> {
        if b.len() < 6 || &b[0..4] != MAGIC {
            return Err(LumenError::InvalidArg("bad Start"));
        }
        Ok(Start {
            client_udp_port: u16::from_le_bytes([b[4], b[5]]),
        })
    }
}

/// Frame a message for the control stream: `u16 LE length || payload`.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(2 + payload.len());
    b.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    b.extend_from_slice(payload);
    b
}

/// Async framed-message IO over a quinn stream (`u16 LE length || payload`).
pub mod io {
    /// Read one framed message (bounded at 64 KiB — control messages are tiny).
    pub async fn read_msg(recv: &mut quinn::RecvStream) -> std::io::Result<Vec<u8>> {
        let mut len = [0u8; 2];
        recv.read_exact(&mut len)
            .await
            .map_err(std::io::Error::other)?;
        let n = u16::from_le_bytes(len) as usize;
        let mut buf = vec![0u8; n];
        recv.read_exact(&mut buf)
            .await
            .map_err(std::io::Error::other)?;
        Ok(buf)
    }

    /// Write one framed message.
    pub async fn write_msg(send: &mut quinn::SendStream, payload: &[u8]) -> std::io::Result<()> {
        send.write_all(&super::frame(payload))
            .await
            .map_err(std::io::Error::other)
    }
}

/// quinn endpoint constructors (host self-signed; client accepts-any — seed-stage trust).
pub mod endpoint {
    use std::sync::Arc;

    /// Server endpoint with a fresh self-signed certificate.
    pub fn server(addr: std::net::SocketAddr) -> anyhow_result::Result<quinn::Endpoint> {
        let cert = rcgen::generate_simple_self_signed(vec!["lumen".into()])
            .map_err(|e| anyhow_result::Error::msg(format!("self-signed cert: {e}")))?;
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
        let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let server_config =
            quinn::ServerConfig::with_single_cert(vec![cert_der], key_der.into())
                .map_err(|e| anyhow_result::Error::msg(format!("server config: {e}")))?;
        Ok(quinn::Endpoint::server(server_config, addr)?)
    }

    /// Client endpoint that skips certificate verification (seed stage; pinning lands with
    /// the pairing/trust model).
    pub fn client_insecure() -> anyhow_result::Result<quinn::Endpoint> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rustls_cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipVerify))
            .with_no_client_auth();
        let quic_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)
            .map_err(|e| anyhow_result::Error::msg(format!("quic client config: {e}")))?;
        let mut ep = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())?;
        ep.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_cfg)));
        Ok(ep)
    }

    /// Minimal error plumbing without pulling anyhow into lumen-core's public API.
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

    #[derive(Debug)]
    struct SkipVerify;

    impl rustls::client::danger::ServerCertVerifier for SkipVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error>
        {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_roundtrip() {
        let w = Welcome {
            abi_version: 1,
            udp_port: 9999,
            mode: Mode {
                width: 2560,
                height: 1440,
                refresh_hz: 240,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [7u8; 16],
            salt: [1, 2, 3, 4],
            frames: 600,
        };
        assert_eq!(Welcome::decode(&w.encode()).unwrap(), w);
    }

    #[test]
    fn hello_start_roundtrip() {
        let h = Hello {
            abi_version: 1,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 120,
            },
        };
        assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
        let s = Start {
            client_udp_port: 1234,
        };
        assert_eq!(Start::decode(&s.encode()).unwrap(), s);
    }
}
