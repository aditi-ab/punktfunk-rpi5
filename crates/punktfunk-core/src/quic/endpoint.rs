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
/// the keep-alive holds it open through quiet control periods. Clamped to a 1 s..1 h window:
/// the ceiling keeps an absurd operator-supplied value inside QUIC's VarInt millisecond range,
/// so the conversion below genuinely cannot fail (it used to panic host startup instead).
fn stream_transport_idle(idle: std::time::Duration) -> Arc<quinn::TransportConfig> {
    use std::time::Duration;
    let idle = idle.clamp(Duration::from_secs(1), Duration::from_secs(3600));
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
    //
    // THE LOSSLESS PLANE SHARES THIS BUFFER, and its frames are ~10× larger: a 48 kHz/24-bit
    // stereo frame is 1 152 B against Opus's ~100 B, so 4 KiB is ~3.5 PCM frames (~14 ms) where
    // it is ~40 Opus frames (~200 ms). That is deliberately NOT raised, for two reasons.
    //
    // (a) It stays faithful to the rule above. Fewer frames of slack is still "shed oldest under
    //     congestion", and one shed PCM frame is exactly what `audio::pcm::PcmConceal` exists to
    //     cover. Trading it for lag would invert the property this cap was introduced to get.
    // (b) A blanket raise would REGRESS the Opus plane. Sizing for six PCM frames (~16 KiB) would
    //     make an Opus session's worst-case backlog ~800 ms — four times what this line was
    //     written to prevent — and the two planes cannot be sized separately here, because
    //     `TransportConfig` is built before the handshake resolves which plane the session runs.
    //     Sizing it per-session means building the transport config after negotiation, which is a
    //     larger change than the hi-res programme needed.
    //
    // ⚠ AND THE EVICTION IS INVISIBLE. `Connection::send_datagram` calls `send(data, drop=true)`,
    // which silently evicts the oldest queued datagrams at the cap and returns `Ok(())` — "buffer
    // full" is not one of `SendDatagramError`'s four variants (`UnsupportedByPeer`, `Disabled`,
    // `TooLarge`, `ConnectionLost`). So no caller can count these drops today; the host's audio
    // egress counters cover `TooLarge` only. Observing them needs `datagram_send_buffer_space()`
    // before the send, or `send_datagram_wait`. Worth knowing before anyone reads a clean drop
    // counter as proof the plane is not shedding.
    t.datagram_send_buffer_size(4 * 1024);
    // MTU discovery probes up to EXACTLY the sealed size of a full IPv4 video datagram (1472)
    // instead of quinn's stock 1452. Two reasons: (a) on a clean 1500-MTU path QUIC gets the
    // last 20 bytes per packet; (b) the ceiling turns discovery into a video-path verdict the
    // host's wire-MTU watcher reads (`punktfunk-host` `native/wire_mtu.rs`) — settled == ceiling
    // proves the path carries full-size video datagrams, settled BELOW it proves it cannot (a
    // VPN/overlay adapter at MTU ~1280 blackholes every video packet while all the small flows
    // pass: the "connects fine, black screen forever" field shape). With the stock 1452 ceiling
    // a healthy path and a constrained one are indistinguishable at the top. This is the ONLY
    // behavioral change on healthy paths, and it's confined to discovery: probes are padded
    // PINGs quinn already expects to lose above a constrained hop — a lost probe settles the
    // search lower, exactly as it did before.
    let mut mtud = quinn::MtuDiscoveryConfig::default();
    // Jumbo opt-in (design/shard-payload-reneg.md Phase 2): with `PUNKTFUNK_JUMBO=1` /
    // `PUNKTFUNK_WIRE_MTU` > 1500 set, discovery probes up to the sealed JUMBO datagram
    // size so a settled connection can PROVE a jumbo path — the actual grow stays
    // client-ack-gated (`native/wire_mtu.rs`). The ceiling is per-ENDPOINT, not
    // per-connection: with the opt-in set, connections to non-jumbo peers spend a few extra
    // failed probes (one PTO each) settling lower; zero cost for anyone who doesn't opt in.
    // Derived with the IPv4 overhead — a v6 peer's sealed jumbo target is smaller, so the
    // ceiling covers it and discovery settles at the v6 path's own budget.
    let probe_ceiling = match crate::config::jumbo_wire_mtu() {
        Some(mtu) => {
            let shard = crate::config::jumbo_shard_payload_for(
                mtu,
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            );
            crate::config::sealed_datagram_bytes(shard) as u16
        }
        None => crate::config::video_datagram_udp_ceiling() as u16,
    };
    mtud.upper_bound(probe_ceiling);
    t.mtu_discovery_config(Some(mtud));
    Arc::new(t)
}

/// Endpoint config for the CLIENT endpoint — the half of the jumbo opt-in that lives on the
/// receiving side, and without which the whole jumbo leg is unreachable.
///
/// `EndpointConfig::max_udp_payload_size` is the QUIC transport parameter this endpoint
/// advertises: "the largest UDP payload I accept". quinn defaults it to **1472** (a 1500-byte
/// Ethernet MTU), and a peer's MTU-discovery search is upper-bounded by
/// `min(MtuDiscoveryConfig::upper_bound, the value the OTHER side advertised)`
/// (`quinn_proto::connection::mtud::SearchState::new`). So raising the host's probe ceiling
/// alone — which is all [`stream_transport_idle`] did — can never make a host's discovery
/// settle above 1472: the *client's* default advertisement caps it, and the host's
/// settled-at-jumbo proof (`native/wire_mtu.rs`, both the mid-session grow and the
/// session-start one) could never fire. This raises the advertisement to the sealed jumbo
/// datagram size so the proof is obtainable at all.
///
/// Gated on the SAME operator opt-in as the probe ceiling ([`crate::config::jumbo_wire_mtu`],
/// i.e. `PUNKTFUNK_JUMBO=1` / `PUNKTFUNK_WIRE_MTU` > 1500) because it is not free: quinn sizes
/// its endpoint receive buffer as `max_udp_payload_size × max_receive_segments × BATCH_SIZE`,
/// which on a GRO-capable Linux/Android client is 64 × 32 segments — ~2.9 MiB at the 1472
/// default, ~18 MiB at jumbo. A jumbo LAN is a deliberate deployment; every other client keeps
/// today's buffer to the byte. Without the opt-in this returns the stock config, so the
/// advertisement, the wire, and the memory are all unchanged.
fn endpoint_config() -> quinn::EndpointConfig {
    let mut cfg = quinn::EndpointConfig::default();
    if let Some(mtu) = crate::config::jumbo_wire_mtu() {
        // Derived exactly like the probe ceiling above (IPv4 overhead — a v6 peer's sealed
        // target is smaller, so this covers it), and clamped into quinn's accepted range.
        let shard = crate::config::jumbo_shard_payload_for(
            mtu,
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        );
        let accept = crate::config::sealed_datagram_bytes(shard).clamp(1200, 65_527) as u16;
        if cfg.max_udp_payload_size(accept).is_ok() {
            tracing::info!(
                max_udp_payload_size = accept,
                wire_mtu = mtu,
                "jumbo opt-in: this endpoint advertises a jumbo QUIC receive ceiling, so the \
                 peer's MTU discovery can prove a jumbo path (it is capped by this value)"
            );
        }
    }
    cfg
}

/// Server endpoint with a fresh self-signed certificate (tests/dev — production hosts
/// persist an identity and use [`server_with_identity`] so clients can pin it).
pub fn server(addr: std::net::SocketAddr) -> anyhow_result::Result<quinn::Endpoint> {
    let cert = rcgen::generate_simple_self_signed(vec!["punktfunk".into()])
        .map_err(|e| anyhow_result::Error::msg(format!("self-signed cert: {e}")))?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
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
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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
    Ok((cert.cert.pem(), cert.signing_key.serialize_pem()))
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
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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

        // `Endpoint::client` hardcodes `EndpointConfig::default()`, whose 1472-byte
        // `max_udp_payload_size` caps the HOST's MTU discovery (see `endpoint_config`), so the
        // endpoint is built by hand to carry the jumbo opt-in. Same bind as before
        // (`0.0.0.0:0`, v4 — no dual-stack flag to reproduce) and the same default runtime.
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let runtime = quinn::default_runtime()
            .ok_or_else(|| anyhow_result::Error::msg("no async runtime found".into()))?;
        let mut ep = quinn::Endpoint::new(endpoint_config(), None, socket, runtime)?;
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
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
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
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
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

    #[test]
    fn absurd_idle_timeout_is_clamped_not_a_panic() {
        // The conversion to quinn's IdleTimeout fails past the QUIC VarInt millisecond
        // ceiling — an operator-supplied huge PUNKTFUNK_IDLE_TIMEOUT_MS used to panic host
        // startup through the `expect`. Both extremes must construct.
        let _ = super::stream_transport_idle(std::time::Duration::MAX);
        let _ = super::stream_transport_idle(std::time::Duration::ZERO);
    }

    /// Where a connection's MTU discovery is allowed to climb to, measured rather than argued
    /// (PW7a). Loopback's own MTU is 64 KiB, so the ONLY thing that can stop the search here is
    /// configuration — which makes this a clean instrument for the two ceilings:
    ///
    /// * **leg A** — server opted in, client NOT: the search stalls at the client's default
    ///   `max_udp_payload_size` advertisement (1472) no matter how high the server's probe
    ///   ceiling is. This is why the shipped jumbo grow could never fire: `wire_mtu.rs` waits
    ///   for a settle at the sealed jumbo size and the peer's transport parameter forbids it.
    /// * **leg B** — both opted in: the search reaches the sealed jumbo datagram, and the
    ///   elapsed time is what the `Welcome`'s bounded proof-wait has to cover.
    ///
    /// `#[ignore]`d: it sets process-wide env (each endpoint reads the opt-in at construction,
    /// which is exactly how the two legs are built) and spends seconds of wall clock.
    /// Run it alone: `cargo test -p punktfunk-core --features quic mtu_discovery -- --ignored
    /// --nocapture --test-threads=1`.
    #[tokio::test]
    #[ignore = "measurement: sets process env and takes ~15 s of wall clock"]
    // Test-only exemption from the crate's `deny(unsafe_code)`: mutating the process
    // environment is `unsafe` in edition 2024, and this measurement's env knob is the
    // documented single-threaded kind.
    #[allow(unsafe_code)]
    async fn mtu_discovery_climbs_only_as_high_as_the_peer_advertises() {
        async fn climb(server_jumbo: bool, client_jumbo: bool) -> (u16, u128) {
            let set = |on: bool| {
                // SAFETY: this `#[ignore]`d measurement is documented above to run alone with
                // `--test-threads=1`, so no concurrent thread reads or writes the environment.
                unsafe {
                    if on {
                        std::env::set_var("PUNKTFUNK_JUMBO", "1");
                    } else {
                        std::env::remove_var("PUNKTFUNK_JUMBO");
                    }
                }
            };
            set(server_jumbo);
            let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = server.local_addr().unwrap();
            set(client_jumbo);
            let client = endpoint::client_insecure().unwrap();
            set(false);
            let accept = tokio::spawn(async move {
                let incoming = server.accept().await.expect("incoming");
                let conn = incoming.await.expect("host side connects");
                (server, conn)
            });
            let client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
            let (_server_ep, host_conn) = accept.await.unwrap();
            // A stream write gives the driver something to transmit, which is what starts the
            // search (probes ride `poll_transmit`); after that each probe's ack drives the next.
            let mut s = host_conn.open_uni().await.unwrap();
            s.write_all(b"go").await.unwrap();
            let want = crate::config::sealed_datagram_bytes(crate::config::jumbo_shard_payload_for(
                9000,
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            )) as u16;
            let t0 = std::time::Instant::now();
            let mut mtu = host_conn.stats().path.current_mtu;
            while t0.elapsed() < std::time::Duration::from_secs(6) && mtu < want {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                mtu = host_conn.stats().path.current_mtu;
            }
            let elapsed = t0.elapsed().as_millis();
            drop(client_conn);
            drop(client);
            (mtu, elapsed)
        }

        let (capped, _) = climb(true, false).await;
        println!("leg A (server opted in, client not): settled at {capped} B UDP payload");
        assert_eq!(
            capped, 1472,
            "a peer that advertises the stock max_udp_payload_size caps the search at 1472 — \
             the whole point of raising it on the client endpoint"
        );

        let (grown, ms) = climb(true, true).await;
        println!("leg B (both opted in): reached {grown} B UDP payload in {ms} ms");
        assert!(
            grown >= 8972,
            "both sides opted in, loopback MTU is 64 KiB — discovery should reach the sealed \
             jumbo datagram, got {grown}"
        );
    }
}
