//! The pairing-ceremony control messages (SPAKE2): PairRequest/Challenge/Proof/Result.

use super::*;
use crate::error::{PunktfunkError, Result};

// ---------------------------------------------------------------------------------------------
// Pairing ceremony (typed control messages): instead of a session Hello, a client may open
// the control stream with PairRequest. The host shows a short PIN out-of-band (log/UI); the
// user types it into the client.
//
// Trust is established by **SPAKE2** (a balanced PAKE), NOT a hash of the PIN. SPAKE2 turns
// the low-entropy PIN into a high-entropy shared key via a Diffie-Hellman exchange; the only
// thing an active man-in-the-middle who terminates the (TOFU) ceremony learns is whether a
// single PIN guess was right — there is no transcript value that reveals the PIN to an
// *offline* dictionary search (the fatal flaw of an HMAC-of-PIN proof over a 4-digit space).
// Both peers' certificate fingerprints are bound in as the SPAKE2 identities, so the
// established key — and the key-confirmation MACs derived from it — only agree when both
// sides saw the same two certificates. After mutual key confirmation the host persists the
// client's fingerprint and the client pins the host's.
// ---------------------------------------------------------------------------------------------

/// Type byte of [`PairRequest`].
pub const MSG_PAIR_REQUEST: u8 = 0x10;
/// Type byte of [`PairChallenge`].
pub const MSG_PAIR_CHALLENGE: u8 = 0x11;
/// Type byte of [`PairProof`].
pub const MSG_PAIR_PROOF: u8 = 0x12;
/// Type byte of [`PairResult`].
pub const MSG_PAIR_RESULT: u8 = 0x13;

/// `client → host`: begin pairing. `name` is the human label the host stores (≤64 bytes
/// UTF-8); `spake_a` is the client's SPAKE2 message (see [`SpakeRole::start`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequest {
    pub name: String,
    pub spake_a: Vec<u8>,
}

/// `host → client`: the host's SPAKE2 message + its key-confirmation MAC. The client
/// finishes SPAKE2, verifies `confirm` (proving the host derived the same key, i.e. knows
/// the PIN and saw the same certs), then sends its own confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairChallenge {
    pub spake_b: Vec<u8>,
    pub confirm: [u8; 32],
}

/// `client → host`: the client's key-confirmation MAC (its single proof attempt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairProof {
    pub confirm: [u8; 32],
}

/// `host → client`: ceremony outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairResult {
    pub ok: bool,
}

/// A length-prefixed (u16 LE) byte field within a control message.
fn put_bytes(b: &mut Vec<u8>, x: &[u8]) {
    b.extend_from_slice(&(x.len() as u16).to_le_bytes());
    b.extend_from_slice(x);
}

/// Read a length-prefixed field at `off`, returning the bytes and the next offset.
fn get_bytes(b: &[u8], off: usize) -> Result<(&[u8], usize)> {
    if off + 2 > b.len() {
        return Err(PunktfunkError::InvalidArg("truncated field"));
    }
    let n = u16::from_le_bytes([b[off], b[off + 1]]) as usize;
    let start = off + 2;
    if start + n > b.len() {
        return Err(PunktfunkError::InvalidArg("field overruns message"));
    }
    Ok((&b[start..start + n], start + n))
}

impl PairRequest {
    pub fn encode(&self) -> Vec<u8> {
        // Same cap, same rule as Hello's copy of this field: truncate on a char boundary —
        // a raw byte cut mid-sequence put invalid UTF-8 on the wire, and the host showed the
        // name with a permanent replacement char in its paired-clients list.
        let name = super::handshake::truncate_to(&self.name, HELLO_NAME_MAX).as_bytes();
        let n = name.len();
        let mut b = Vec::with_capacity(8 + n + self.spake_a.len());
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_REQUEST);
        b.push(n as u8);
        b.extend_from_slice(name);
        put_bytes(&mut b, &self.spake_a);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairRequest> {
        if b.len() < 6 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_REQUEST {
            return Err(PunktfunkError::InvalidArg("bad PairRequest"));
        }
        let n = b[5] as usize;
        if n > 64 || b.len() < 6 + n {
            return Err(PunktfunkError::InvalidArg("bad PairRequest name"));
        }
        let name = String::from_utf8_lossy(&b[6..6 + n]).into_owned();
        let (spake_a, end) = get_bytes(b, 6 + n)?;
        if end != b.len() {
            return Err(PunktfunkError::InvalidArg("trailing bytes"));
        }
        Ok(PairRequest {
            name,
            spake_a: spake_a.to_vec(),
        })
    }
}

impl PairChallenge {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(7 + self.spake_b.len() + 32);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_CHALLENGE);
        put_bytes(&mut b, &self.spake_b);
        b.extend_from_slice(&self.confirm);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairChallenge> {
        if b.len() < 5 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_CHALLENGE {
            return Err(PunktfunkError::InvalidArg("bad PairChallenge"));
        }
        let (spake_b, end) = get_bytes(b, 5)?;
        if end + 32 != b.len() {
            return Err(PunktfunkError::InvalidArg("bad PairChallenge confirm"));
        }
        let mut confirm = [0u8; 32];
        confirm.copy_from_slice(&b[end..end + 32]);
        Ok(PairChallenge {
            spake_b: spake_b.to_vec(),
            confirm,
        })
    }
}

impl PairProof {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(37);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_PROOF);
        b.extend_from_slice(&self.confirm);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairProof> {
        if b.len() != 37 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_PROOF {
            return Err(PunktfunkError::InvalidArg("bad PairProof"));
        }
        let mut confirm = [0u8; 32];
        confirm.copy_from_slice(&b[5..37]);
        Ok(PairProof { confirm })
    }
}

impl PairResult {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(6);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_RESULT);
        b.push(self.ok as u8);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairResult> {
        if b.len() != 6 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_RESULT {
            return Err(PunktfunkError::InvalidArg("bad PairResult"));
        }
        Ok(PairResult { ok: b[5] != 0 })
    }
}

#[cfg(test)]
mod tests {
    use crate::quic::*;

    #[test]
    fn pair_messages_roundtrip() {
        let pr = PairRequest {
            name: "Enrico's Mac".into(),
            spake_a: vec![1, 2, 3, 4, 5],
        };
        assert_eq!(PairRequest::decode(&pr.encode()).unwrap(), pr);
        let pc = PairChallenge {
            spake_b: vec![9; 33],
            confirm: [7u8; 32],
        };
        assert_eq!(PairChallenge::decode(&pc.encode()).unwrap(), pc);
        let pp = PairProof { confirm: [3u8; 32] };
        assert_eq!(PairProof::decode(&pp.encode()).unwrap(), pp);
        for ok in [true, false] {
            assert_eq!(
                PairResult::decode(&PairResult { ok }.encode()).unwrap().ok,
                ok
            );
        }
        // Length-exact: a truncated/padded PairProof is rejected.
        let mut bad = pp.encode();
        bad.push(0);
        assert!(PairProof::decode(&bad).is_err());
    }

    #[test]
    fn pair_request_name_cap_respects_char_boundaries() {
        // A multi-byte char straddling the 64-byte cap must be dropped whole (Hello's rule),
        // not split mid-sequence into invalid UTF-8 the host then renders as U+FFFD forever.
        let pr = PairRequest {
            name: format!("{}\u{00fc}", "x".repeat(HELLO_NAME_MAX - 1)),
            spake_a: vec![1, 2, 3],
        };
        let dec = PairRequest::decode(&pr.encode()).unwrap();
        assert!(dec.name.len() <= HELLO_NAME_MAX && dec.name.starts_with('x'));
        assert!(
            !dec.name.contains('\u{FFFD}'),
            "name must never be split mid-char on the wire"
        );
    }
}
