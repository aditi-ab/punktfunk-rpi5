//! Pairing-ceremony control messages: PairRequest / Challenge / Proof / Result.
//!
//! A client may open the control stream with [`PairRequest`] instead of Hello.
//! The host shows a short PIN out-of-band; the user types it on the client.
//! Trust is [`super::pake`] (SPAKE2), not a hash of the PIN: an active MITM learns
//! only whether one guess was right — no transcript for offline search.
//! Both certificate fingerprints are SPAKE2 identities, so the confirmation
//! MACs agree only when both sides saw the same two certs. After mutual
//! confirmation the host persists the client's fingerprint and the client
//! pins the host's.

use super::*;
use crate::error::{PunktfunkError, Result};

pub const MSG_PAIR_REQUEST: u8 = 0x10;
pub const MSG_PAIR_CHALLENGE: u8 = 0x11;
pub const MSG_PAIR_PROOF: u8 = 0x12;
pub const MSG_PAIR_RESULT: u8 = 0x13;

/// `client → host`: begin pairing. `name` is the host-stored label (≤64 bytes UTF-8);
/// `spake_a` is the client's SPAKE2 message (see [`super::pake::start`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequest {
    pub name: String,
    pub spake_a: Vec<u8>,
}

/// `host → client`: host SPAKE2 message + key-confirmation MAC. The client
/// finishes SPAKE2, verifies `confirm` (same key ⇒ same PIN and certs), then
/// sends its own confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairChallenge {
    pub spake_b: Vec<u8>,
    pub confirm: [u8; 32],
}

/// `client → host`: client's key-confirmation MAC (one proof attempt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairProof {
    pub confirm: [u8; 32],
}

/// `host → client`: ceremony outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairResult {
    pub ok: bool,
}

fn put_bytes(b: &mut Vec<u8>, x: &[u8]) {
    b.extend_from_slice(&(x.len() as u16).to_le_bytes());
    b.extend_from_slice(x);
}

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
        // Same cap as Hello: truncate on a char boundary. A mid-sequence cut
        // puts invalid UTF-8 on the wire and the host stores U+FFFD forever.
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
        let mut bad = pp.encode();
        bad.push(0);
        assert!(PairProof::decode(&bad).is_err());
    }

    #[test]
    fn pair_request_name_cap_respects_char_boundaries() {
        // Drop a straddling multi-byte char whole (Hello's rule), never split
        // into invalid UTF-8 that the host would store as U+FFFD.
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
