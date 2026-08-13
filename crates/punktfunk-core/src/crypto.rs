//! Session sealing with the negotiated AEAD — AES-128-GCM (matching GameStream's video
//! crypto in P1) by default, ChaCha20-Poly1305 (RFC 8439) for clients without hardware AES.
//!
//! ## Nonce uniqueness (the AEAD safety requirement)
//!
//! The 96-bit nonce is `salt (4 bytes) || sequence (8 bytes, big-endian)`. Reusing a
//! `(key, nonce)` pair is catastrophic under either AEAD, so two precautions apply:
//!
//! 1. **Per-direction salts.** Host and client share one `key` and `salt`, and each
//!    counts its sequence from 0. To stop the host's video stream and the client's input
//!    stream from colliding on `(key, nonce)`, the top bit of `salt[0]` is set to the
//!    sender's direction — so the two directions occupy disjoint nonce spaces.
//! 2. **Per-session key+salt.** The pairing layer MUST hand each session a fresh
//!    `(key, salt)`; reusing them across sessions reintroduces nonce reuse. `Config`'s
//!    all-zero key with `encrypt = true` is rejected by `Config::validate` to catch the
//!    obvious footgun.
//!
//! The sequence number is also passed as AEAD associated data, so tampering with the
//! on-wire sequence is detected (the tag check fails) rather than silently shifting the
//! nonce. Note: this layer does not provide anti-replay — see `Session`.
//!
//! ## Why two ciphers
//!
//! Both AEADs are full-strength; the choice (negotiated via `Welcome::cipher`) is purely a
//! performance one. On targets without hardware AES — the soft-AES armv7 clients (webOS TVs) —
//! GCM's fixsliced AES + software GHASH costs ~50–100 cycles/byte and caps decrypt at
//! ~100 Mbps, while ChaCha20-Poly1305's ARX construction runs ~10–17 cycles/byte in portable
//! software (design/chacha20-session-cipher.md). Same 96-bit nonce, 16-byte tag, and AAD
//! shape, so the entire nonce discipline above carries over verbatim.

use crate::config::Role;
use crate::error::{PunktfunkError, Result};
use aes_gcm::aead::{Aead, AeadInOut, KeyInit, Payload};
use aes_gcm::Aes128Gcm;
use chacha20poly1305::ChaCha20Poly1305;
use zeroize::Zeroize;

/// 16-byte AEAD authentication tag appended by either session cipher.
pub const TAG_LEN: usize = 16;

// The wire (CRYPTO_OVERHEAD) and every in-place split assume both negotiated AEADs append
// exactly TAG_LEN bytes — a different-tag cipher can never slip in behind this constant.
const _: () = assert!(std::mem::size_of::<aes_gcm::Tag>() == TAG_LEN);
const _: () = assert!(std::mem::size_of::<chacha20poly1305::Tag>() == TAG_LEN);

/// The negotiated session AEAD together with its key material — merged so the invalid state
/// (a ChaCha cipher with an AES-sized key, or vice versa) is unrepresentable. AES-128-GCM is
/// the default every peer speaks; ChaCha20-Poly1305 is granted to clients that advertised
/// [`VIDEO_CAP_CHACHA20`](crate::quic::VIDEO_CAP_CHACHA20) (the soft-AES armv7 targets —
/// see the module docs). 256 bits for ChaCha is what RFC 8439 requires.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionKey {
    Aes128Gcm([u8; 16]),
    ChaCha20Poly1305([u8; 32]),
}

impl SessionKey {
    /// Canonical lowercase cipher name for session-start logs.
    pub fn cipher_name(&self) -> &'static str {
        match self {
            SessionKey::Aes128Gcm(_) => "aes-128-gcm",
            SessionKey::ChaCha20Poly1305(_) => "chacha20-poly1305",
        }
    }

    /// True when the key material is all zeros — the pairing-layer footgun `Config::validate`
    /// rejects when encryption is on (see the nonce-uniqueness contract in the module docs).
    pub fn is_zero(&self) -> bool {
        match self {
            SessionKey::Aes128Gcm(k) => k == &[0u8; 16],
            SessionKey::ChaCha20Poly1305(k) => k == &[0u8; 32],
        }
    }
}

/// Key material never appears in logs, whichever variant is active — only the cipher choice
/// (`Config`'s hand-written `Debug` relies on this).
impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionKey::Aes128Gcm(_) => f.write_str("Aes128Gcm(<redacted>)"),
            SessionKey::ChaCha20Poly1305(_) => f.write_str("ChaCha20Poly1305(<redacted>)"),
        }
    }
}

/// Same zeroize-on-drop discipline the raw key array had (`Config`'s `Drop`).
impl Zeroize for SessionKey {
    fn zeroize(&mut self) {
        match self {
            SessionKey::Aes128Gcm(k) => k.zeroize(),
            SessionKey::ChaCha20Poly1305(k) => k.zeroize(),
        }
    }
}

/// The two negotiated AEADs behind one seal/open surface. Both are the same RustCrypto
/// `aead 0.6` generation (identical trait shapes, nonce/tag types), so each call below is a
/// two-arm match right next to the cipher work itself.
// AES's precomputed round keys (~0.7 KB) dwarf ChaCha's 32-byte state, but there is exactly
// one long-lived `SessionCrypto` per session — boxing the variant would trade that one-off
// slack for a pointer chase on every per-datagram seal/open.
#[allow(clippy::large_enum_variant)]
enum Cipher {
    Aes128Gcm(Aes128Gcm),
    ChaCha20Poly1305(ChaCha20Poly1305),
}

pub struct SessionCrypto {
    cipher: Cipher,
    /// Salt for nonces we seal with (our direction).
    send_salt: [u8; 4],
    /// Salt for nonces we open with (the peer's direction).
    recv_salt: [u8; 4],
}

impl SessionCrypto {
    pub fn new(key: &SessionKey, salt: [u8; 4], role: Role) -> Self {
        let cipher = match key {
            // `&[u8; N] -> &Array<u8, UN>` is a checked-at-compile-time reference cast (the
            // `hybrid_array` successor to `generic-array`'s runtime-length `from_slice`).
            SessionKey::Aes128Gcm(k) => Cipher::Aes128Gcm(Aes128Gcm::new(k.into())),
            SessionKey::ChaCha20Poly1305(k) => {
                Cipher::ChaCha20Poly1305(ChaCha20Poly1305::new(k.into()))
            }
        };
        let own = direction(role);
        SessionCrypto {
            cipher,
            send_salt: dir_salt(salt, own),
            recv_salt: dir_salt(salt, own ^ 1),
        }
    }

    /// Seal `plaintext` for sequence `seq`, returning `ciphertext || tag`. `seq` is
    /// authenticated as associated data.
    pub fn seal(&self, seq: u64, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = nonce(self.send_salt, seq);
        let aad = seq.to_be_bytes();
        let payload = Payload {
            msg: plaintext,
            aad: &aad,
        };
        match &self.cipher {
            Cipher::Aes128Gcm(c) => c.encrypt((&nonce).into(), payload),
            Cipher::ChaCha20Poly1305(c) => c.encrypt((&nonce).into(), payload),
        }
        .map_err(|_| PunktfunkError::Crypto)
    }

    /// Seal in place, no per-packet allocation: `buf` is laid out as `[plaintext .. ][TAG_LEN]` (the
    /// last `TAG_LEN` bytes are scratch); on return it holds `[ciphertext .. ][tag]` — byte-identical
    /// to `seal`'s `ciphertext || tag`, just written in place. The hot-path sealer (`Session`) uses
    /// this to avoid the `Vec` that `seal`'s convenience API allocates for every packet.
    pub fn seal_in_place(&self, seq: u64, buf: &mut [u8]) -> Result<()> {
        debug_assert!(buf.len() >= TAG_LEN);
        let nonce = nonce(self.send_salt, seq);
        let split = buf.len() - TAG_LEN;
        let (plaintext, tag_slot) = buf.split_at_mut(split);
        let aad = seq.to_be_bytes();
        let tag = match &self.cipher {
            Cipher::Aes128Gcm(c) => {
                c.encrypt_inout_detached((&nonce).into(), &aad, plaintext.into())
            }
            Cipher::ChaCha20Poly1305(c) => {
                c.encrypt_inout_detached((&nonce).into(), &aad, plaintext.into())
            }
        }
        .map_err(|_| PunktfunkError::Crypto)?;
        tag_slot.copy_from_slice(&tag);
        Ok(())
    }

    /// Open `ciphertext || tag` for sequence `seq` (also bound as associated data).
    pub fn open(&self, seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let nonce = nonce(self.recv_salt, seq);
        let aad = seq.to_be_bytes();
        let payload = Payload {
            msg: ciphertext,
            aad: &aad,
        };
        match &self.cipher {
            Cipher::Aes128Gcm(c) => c.decrypt((&nonce).into(), payload),
            Cipher::ChaCha20Poly1305(c) => c.decrypt((&nonce).into(), payload),
        }
        .map_err(|_| PunktfunkError::Crypto)
    }

    /// Open in place, no per-packet allocation: `buf` holds `[ciphertext .. ][tag]` on entry and
    /// the plaintext in its first `buf.len() - TAG_LEN` bytes on success (returned as the length)
    /// — byte-identical to `open`, just written in place. Both AEADs verify the tag *before*
    /// decrypting, so on failure `buf` still holds the ciphertext (the caller drops the packet
    /// either way). The hot-path receiver (`Session::poll_frame`) uses this to avoid the `Vec`
    /// that `open`'s convenience API allocates for every datagram at line rate — the receive
    /// mirror of [`seal_in_place`](Self::seal_in_place).
    pub fn open_in_place(&self, seq: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.len() < TAG_LEN {
            return Err(PunktfunkError::BadPacket);
        }
        let nonce = nonce(self.recv_salt, seq);
        let split = buf.len() - TAG_LEN;
        let (ciphertext, tag) = buf.split_at_mut(split);
        let aad = seq.to_be_bytes();
        // `split_at_mut` above already fixed this at exactly TAG_LEN, and the two AEADs share the
        // one 16-byte tag type (the const asserts at the top of the module), so this reference cast
        // is infallible and serves both arms — mapped rather than unwrapped to keep the hot path
        // panic-free.
        let tag: &aes_gcm::Tag = (&*tag).try_into().map_err(|_| PunktfunkError::Crypto)?;
        match &self.cipher {
            Cipher::Aes128Gcm(c) => {
                c.decrypt_inout_detached((&nonce).into(), &aad, ciphertext.into(), tag)
            }
            Cipher::ChaCha20Poly1305(c) => {
                c.decrypt_inout_detached((&nonce).into(), &aad, ciphertext.into(), tag)
            }
        }
        .map_err(|_| PunktfunkError::Crypto)?;
        Ok(split)
    }
}

fn direction(role: Role) -> u8 {
    match role {
        Role::Host => 0,
        Role::Client => 1,
    }
}

/// Fold a 1-bit direction into the salt (top bit of `salt[0]`) so the two directions of
/// a session never share a nonce under the same key.
fn dir_salt(mut salt: [u8; 4], dir: u8) -> [u8; 4] {
    salt[0] = (salt[0] & 0x7f) | (dir << 7);
    salt
}

fn nonce(salt: [u8; 4], seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&salt);
    n[4..].copy_from_slice(&seq.to_be_bytes());
    n
}

/// Generate a fresh random AES-128 session key (control-plane / pairing use).
pub fn random_key() -> [u8; 16] {
    let mut k = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut k);
    k
}

/// Generate a fresh random ChaCha20-Poly1305 session key (RFC 8439's 256-bit size).
pub fn random_key32() -> [u8; 32] {
    let mut k = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut k);
    k
}

/// Generate a fresh random per-session nonce salt.
pub fn random_salt() -> [u8; 4] {
    let mut s = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One fresh key per negotiated cipher — every sealing test below must hold for both.
    fn both_keys() -> [SessionKey; 2] {
        [
            SessionKey::Aes128Gcm(random_key()),
            SessionKey::ChaCha20Poly1305(random_key32()),
        ]
    }

    #[test]
    fn seal_open_roundtrip_cross_direction() {
        for key in both_keys() {
            let salt = random_salt();
            let host = SessionCrypto::new(&key, salt, Role::Host);
            let client = SessionCrypto::new(&key, salt, Role::Client);

            let msg = b"the quick brown fox";
            let sealed = host.seal(42, msg).unwrap(); // host -> client (video direction)
            assert_ne!(&sealed[..msg.len()], &msg[..]); // actually encrypted
            assert_eq!(sealed.len(), msg.len() + TAG_LEN);
            assert_eq!(client.open(42, &sealed).unwrap(), msg);

            // Wrong sequence (nonce + AAD) → authentication failure.
            assert!(client.open(43, &sealed).is_err());
            // Direction separation: the host opens with the peer (client) salt, so it cannot
            // open its own outbound packet → distinct nonce spaces per direction.
            assert!(host.open(42, &sealed).is_err());
        }
    }

    #[test]
    fn directions_use_distinct_nonce_spaces() {
        for key in both_keys() {
            let salt = [0u8; 4]; // even an all-zero base salt must separate the directions
            let host = SessionCrypto::new(&key, salt, Role::Host);
            let client = SessionCrypto::new(&key, salt, Role::Client);
            // Same seq, same key, opposite directions → different ciphertext (no reuse).
            assert_ne!(
                host.seal(0, b"abc").unwrap(),
                client.seal(0, b"abc").unwrap()
            );
        }
    }

    #[test]
    fn open_in_place_matches_open_and_rejects_tampering() {
        for key in both_keys() {
            let salt = random_salt();
            let host = SessionCrypto::new(&key, salt, Role::Host);
            let client = SessionCrypto::new(&key, salt, Role::Client);
            for msg in [
                &b""[..],
                b"x",
                b"the quick brown fox jumps over 13 lazy dogs!!",
            ] {
                let sealed = host.seal(9, msg).unwrap();
                let mut buf = sealed.clone();
                let n = client.open_in_place(9, &mut buf).unwrap();
                assert_eq!(
                    &buf[..n],
                    msg,
                    "in-place open must be byte-identical to open"
                );
                // Wrong sequence (nonce + AAD) → authentication failure, like `open`.
                let mut buf = sealed.clone();
                assert!(client.open_in_place(8, &mut buf).is_err());
                // A flipped ciphertext/tag bit → authentication failure.
                let mut buf = sealed.clone();
                let last = buf.len() - 1;
                buf[last] ^= 1;
                assert!(client.open_in_place(9, &mut buf).is_err());
            }
            // Shorter than a tag can't be a sealed packet at all.
            let mut runt = vec![0u8; TAG_LEN - 1];
            assert!(client.open_in_place(0, &mut runt).is_err());
        }
    }

    #[test]
    fn seal_in_place_matches_seal_and_opens() {
        for key in both_keys() {
            let salt = random_salt();
            let host = SessionCrypto::new(&key, salt, Role::Host);
            let client = SessionCrypto::new(&key, salt, Role::Client);
            for msg in [
                &b""[..],
                b"x",
                b"the quick brown fox jumps over 13 lazy dogs!!",
            ] {
                let reference = host.seal(7, msg).unwrap(); // ciphertext || tag
                                                            // In-place: [plaintext .. ][TAG_LEN scratch].
                let mut buf = msg.to_vec();
                buf.resize(msg.len() + TAG_LEN, 0);
                host.seal_in_place(7, &mut buf).unwrap();
                assert_eq!(
                    buf, reference,
                    "in-place seal must be byte-identical to seal"
                );
                assert_eq!(client.open(7, &buf).unwrap(), msg);
            }
        }
    }

    #[test]
    fn ciphers_are_not_interchangeable() {
        // A packet sealed under one AEAD must not open under the other — negotiation skew has
        // to fail loudly (a tag mismatch), never decode garbage. The ChaCha key repeats the AES
        // key bytes so even overlapping key material can't accidentally interoperate.
        let salt = random_salt();
        let aes = SessionKey::Aes128Gcm([7u8; 16]);
        let chacha = SessionKey::ChaCha20Poly1305([7u8; 32]);
        let sealed = SessionCrypto::new(&aes, salt, Role::Host)
            .seal(1, b"cross-cipher")
            .unwrap();
        assert!(SessionCrypto::new(&chacha, salt, Role::Client)
            .open(1, &sealed)
            .is_err());
        let sealed = SessionCrypto::new(&chacha, salt, Role::Host)
            .seal(1, b"cross-cipher")
            .unwrap();
        assert!(SessionCrypto::new(&aes, salt, Role::Client)
            .open(1, &sealed)
            .is_err());
    }

    #[test]
    fn session_key_zero_check_and_debug_redaction() {
        assert!(SessionKey::Aes128Gcm([0u8; 16]).is_zero());
        assert!(SessionKey::ChaCha20Poly1305([0u8; 32]).is_zero());
        assert!(!SessionKey::Aes128Gcm([1u8; 16]).is_zero());
        assert!(!SessionKey::ChaCha20Poly1305([1u8; 32]).is_zero());
        // Key bytes must never reach a log, whichever variant — only the cipher choice.
        for key in both_keys() {
            let dbg = format!("{key:?}");
            assert!(dbg.contains("<redacted>"), "{dbg}");
        }
        let mut k = SessionKey::ChaCha20Poly1305([9u8; 32]);
        k.zeroize();
        assert!(k.is_zero());
    }
}
