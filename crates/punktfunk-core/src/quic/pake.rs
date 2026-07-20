//! SPAKE2 password-authenticated key exchange for pairing: derive a shared key from the PIN and
//! confirm it against both certificate fingerprints.
use crate::error::{PunktfunkError, Result};
use hmac::{Hmac, Mac};
use spake2::{Ed25519Group, Identity, Password, Spake2};

/// In-progress SPAKE2 state plus the identity transcript for key confirmation.
pub struct PairingPake {
    state: Spake2<Ed25519Group>,
    transcript: Vec<u8>,
}

/// Start the exchange. `client_fp`/`host_fp` are the two certificate fingerprints (the
/// client passes what it observed via TOFU; the host passes its own + the client's
/// presented cert). Returns the state and this side's outbound SPAKE2 message.
pub fn start(
    is_client: bool,
    pin: &str,
    client_fp: &[u8; 32],
    host_fp: &[u8; 32],
) -> (PairingPake, Vec<u8>) {
    let pw = Password::new(pin.as_bytes());
    let id_client = Identity::new(client_fp);
    let id_host = Identity::new(host_fp);
    let (state, msg) = if is_client {
        Spake2::<Ed25519Group>::start_a(&pw, &id_client, &id_host)
    } else {
        Spake2::<Ed25519Group>::start_b(&pw, &id_client, &id_host)
    };
    let mut transcript = Vec::with_capacity(64);
    transcript.extend_from_slice(client_fp);
    transcript.extend_from_slice(host_fp);
    (PairingPake { state, transcript }, msg)
}

/// Key confirmation MAC for one direction (`label` distinguishes host vs client), keyed
/// by the SPAKE2 shared key and bound to the fingerprint transcript.
fn confirm(key: &[u8], label: &[u8], transcript: &[u8]) -> [u8; 32] {
    let mut mac =
        <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("hmac takes any key length");
    mac.update(label);
    mac.update(transcript);
    mac.finalize().into_bytes().into()
}

/// `Hmac` verification is constant-time via `ct_eq` in the underlying crate; we compare
/// our recomputed tag the same way.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Confirmation tags both sides expect, given the agreed SPAKE2 key.
pub struct Confirmations {
    /// MAC the host sends (client verifies).
    pub host: [u8; 32],
    /// MAC the client sends (host verifies).
    pub client: [u8; 32],
}

impl PairingPake {
    /// Finish SPAKE2 with the peer's message → the pair of confirmation tags. `Err` if
    /// the peer's message is malformed (a wrong PIN does NOT error here — it yields a
    /// *different* key, so the confirmation MACs simply won't match).
    pub fn finish(self, peer_msg: &[u8]) -> Result<Confirmations> {
        let key = self
            .state
            .finish(peer_msg)
            .map_err(|_| PunktfunkError::Crypto)?;
        Ok(Confirmations {
            host: confirm(&key, b"punktfunk-pair-host", &self.transcript),
            client: confirm(&key, b"punktfunk-pair-client", &self.transcript),
        })
    }
}

/// Constant-time tag comparison for the confirmation step.
pub fn verify(expected: &[u8; 32], got: &[u8; 32]) -> bool {
    ct_eq(expected, got)
}

#[cfg(test)]
mod tests {
    use crate::quic::pake;

    #[test]
    fn spake2_pairing_agrees_only_on_matching_pin_and_certs() {
        let cfp = [0x11u8; 32];
        let hfp = [0x22u8; 32];

        // Right PIN, same fingerprint views on both sides → both confirmations agree.
        let (ca, ma) = pake::start(true, "4321", &cfp, &hfp);
        let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
        let a = ca.finish(&mb).unwrap();
        let b = cb.finish(&ma).unwrap();
        assert!(pake::verify(&a.host, &b.host) && pake::verify(&a.client, &b.client));

        // Wrong PIN → different keys → confirmations DON'T match (one online guess wasted).
        let (ca, ma) = pake::start(true, "0000", &cfp, &hfp);
        let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
        let a = ca.finish(&mb).unwrap();
        let b = cb.finish(&ma).unwrap();
        assert!(!pake::verify(&a.client, &b.client));

        // MITM: the two legs saw different host certs → no agreement even with the right PIN.
        let attacker_hfp = [0x33u8; 32];
        let (ca, ma) = pake::start(true, "4321", &cfp, &attacker_hfp);
        let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
        let a = ca.finish(&mb).unwrap();
        let b = cb.finish(&ma).unwrap();
        assert!(!pake::verify(&a.client, &b.client));
    }
}
