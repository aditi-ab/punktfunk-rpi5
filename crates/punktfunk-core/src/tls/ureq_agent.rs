//! A blocking [`ureq::Agent`] that speaks TLS through a caller-supplied
//! [`rustls::ClientConfig`] — which is the only way to get [`PinVerify`](super::PinVerify) into an
//! HTTP client, because ureq's own `TlsConfig` exposes roots, a client cert and an
//! off-switch, but no hook for a custom [`ServerCertVerifier`](rustls::client::danger::ServerCertVerifier).
//!
//! Every caller here pins the host's self-signed leaf by fingerprint (the same trust rule as the
//! QUIC plane), so "just use the default agent" is not an option: the default agent validates
//! against webpki roots, which a self-signed host cert can never satisfy.
//!
//! The connector below is modelled on ureq 3.x's own (crate-private) `RustlsConnector` minus its
//! `TlsConfig`-driven config-building step. It is transport glue, not crypto: the handshake, the
//! verifier and the cipher suites all live in the `ClientConfig` the caller hands in.

use std::io::{Read as _, Write as _};
use std::sync::Arc;

use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, TcpConnector,
    Transport, TransportAdapter,
};

/// Build an agent whose HTTPS connections use `tls` verbatim, with `config` for everything else
/// (timeouts, redirect policy, buffer sizes) — built by the caller via
/// [`ureq::Agent::config_builder`], since those knobs differ per call site.
pub fn agent(tls: Arc<rustls::ClientConfig>, config: ureq::config::Config) -> ureq::Agent {
    let connector = TcpConnector::default().chain(PinnedTlsConnector { config: tls });
    ureq::Agent::with_parts(config, connector, DefaultResolver::default())
}

struct PinnedTlsConnector {
    config: Arc<rustls::ClientConfig>,
}

impl std::fmt::Debug for PinnedTlsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedTlsConnector").finish()
    }
}

impl<In: Transport> Connector<In> for PinnedTlsConnector {
    type Out = Either<In, PinnedTlsTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else {
            // Unreachable via `agent()` above, which always chains onto a TcpConnector.
            return Err(ureq::Error::Tls("no chained transport to wrap in TLS"));
        };
        // A plain-HTTP URL, or something that already negotiated TLS, passes straight through.
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(Either::A(transport)));
        }

        let name: rustls::pki_types::ServerName<'_> = details
            .uri
            .authority()
            .ok_or(ureq::Error::Tls("uri has no authority"))?
            .host()
            .try_into()
            .map_err(|_| ureq::Error::Tls("invalid DNS name"))?;
        let conn = rustls::ClientConnection::new(self.config.clone(), name.to_owned())?;
        let stream = rustls::StreamOwned {
            conn,
            sock: TransportAdapter::new(transport.boxed()),
        };
        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(Either::B(PinnedTlsTransport { buffers, stream })))
    }
}

struct PinnedTlsTransport {
    buffers: LazyBuffers,
    stream: rustls::StreamOwned<rustls::ClientConnection, TransportAdapter>,
}

impl std::fmt::Debug for PinnedTlsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedTlsTransport").finish()
    }
}

impl Transport for PinnedTlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let output = &self.buffers.output()[..amount];
        self.stream.write_all(output)?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let input = self.buffers.input_append_buf();
        let amount = self.stream.read(input)?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}
