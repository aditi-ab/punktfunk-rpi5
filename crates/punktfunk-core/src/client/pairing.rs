//! The client-side PIN pairing ceremony (SPAKE2): `NativeClient::pair`.

use super::worker::reject_from_close;
use super::{join_host_port, NativeClient};
use crate::error::{PunktfunkError, Result};
use crate::quic::{endpoint, io};
use std::time::Duration;

impl NativeClient {
    /// Run the PIN pairing ceremony against a host: connect (trust-on-first-use — the PIN
    /// proof is what authenticates the certificates), prove knowledge of the PIN the host
    /// is displaying, and return the host's now-verified fingerprint for pinning. The host
    /// persists this client's fingerprint in its paired set.
    ///
    /// `identity` is this client's persistent PEM identity (cert, key) — the same one
    /// later passed to [`NativeClient::connect`]; `pin` is what the user read off the host
    /// (its log / UI); `name` is the label the host stores.
    pub fn pair(
        host: &str,
        port: u16,
        identity: (&str, &str),
        pin: &str,
        name: &str,
        timeout: Duration,
    ) -> Result<[u8; 32]> {
        use crate::quic::{pake, PairChallenge, PairProof, PairRequest, PairResult};

        let client_fp = endpoint::fingerprint_of_pem(identity.0)
            .map_err(|_| PunktfunkError::InvalidArg("client cert pem"))?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(PunktfunkError::Io)?;
        let pin = pin.to_string();
        let name = name.to_string();
        let remote: std::net::SocketAddr = join_host_port(host, port)
            .parse()
            .map_err(|_| PunktfunkError::InvalidArg("host:port"))?;

        rt.block_on(async move {
            // The quinn endpoint must be created inside the runtime (it spawns its driver).
            let (ep, observed) = endpoint::client_pinned_with_identity(None, Some(identity));
            let ep = ep.map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;

            // The SPAKE2 exchange over an already-open bi-stream; never closes the conn (the
            // caller does, then flushes), so any early exit still lets the host see the close.
            let exchange = |conn: quinn::Connection, host_fp: [u8; 32]| async move {
                let (mut send, mut recv) = conn
                    .open_bi()
                    .await
                    .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
                // SPAKE2 as A, binding our fingerprint + the host cert we observed (TOFU).
                let (pake, spake_a) = pake::start(true, &pin, &client_fp, &host_fp);
                io::write_msg(&mut send, &PairRequest { name, spake_a }.encode()).await?;
                let challenge = PairChallenge::decode(&io::read_msg(&mut recv).await?)?;
                let confirms = pake.finish(&challenge.spake_b)?;
                // The host's confirmation proves it reached the same key (right PIN, same
                // certs) — only then do we pin it and send our own confirmation.
                if !pake::verify(&confirms.host, &challenge.confirm) {
                    return Err(PunktfunkError::Crypto); // wrong PIN or MITM
                }
                io::write_msg(
                    &mut send,
                    &PairProof {
                        confirm: confirms.client,
                    }
                    .encode(),
                )
                .await?;
                let result = PairResult::decode(&io::read_msg(&mut recv).await?)?;
                if result.ok {
                    Ok(host_fp)
                } else {
                    Err(PunktfunkError::Crypto) // host rejected post-confirm
                }
            };

            let ceremony = async {
                let conn = ep
                    .connect(remote, "punktfunk")
                    .map_err(|_| PunktfunkError::InvalidArg("connect"))?
                    .await
                    .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
                let host_fp = observed.lock().unwrap().ok_or(PunktfunkError::Crypto)?;
                let outcome = match exchange(conn.clone(), host_fp).await {
                    // A typed application close from the host (pairing not armed / armed for a
                    // different device / rate-limited / version mismatch) beats the generic
                    // transport error the aborted exchange produced — it is the actual answer.
                    Err(e) => Err(match reject_from_close(&conn) {
                        Some(r) => PunktfunkError::Rejected(r),
                        None => e,
                    }),
                    ok => ok,
                };
                // Always tell the host we're done so it never blocks at its read — code 0 on
                // success, 1 on a refused/aborted ceremony.
                let code: u32 = if outcome.is_ok() { 0 } else { 1 };
                conn.close(code.into(), b"pair done");
                outcome
            };
            let outcome = tokio::time::timeout(timeout, ceremony)
                .await
                .map_err(|_| PunktfunkError::Timeout)?;
            // Flush the CONNECTION_CLOSE before the runtime is dropped — otherwise the host
            // may never see it and would block at its read for the full pairing timeout.
            let _ = tokio::time::timeout(Duration::from_secs(2), ep.wait_idle()).await;
            outcome
        })
    }
}
