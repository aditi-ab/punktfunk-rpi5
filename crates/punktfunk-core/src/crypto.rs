//! Session sealing with the negotiated AEAD.
//!
//! AES-128-GCM by default; ChaCha20-Poly1305 for peers without hardware AES.
//! Same 96-bit nonce, 16-byte tag, and AAD shape.
//!
//! Nonce is `salt (4 bytes) || sequence (8 bytes, BE)`. Reusing a
//! `(key, nonce)` pair is catastrophic. Host and client share one key+salt
//! and both count from 0, so `salt[0]`'s top bit is the sender's direction —
//! disjoint nonce spaces. Pairing supplies a fresh `(key, salt)` per session;
//! `Config::validate` rejects an all-zero key when `encrypt` is on.
//!
//! Sequence is also AAD, so a tampered on-wire seq fails the tag instead of
//! shifting the nonce. Anti-replay lives in `Session`, not here.

use crate::config::Role;
use crate::error::{PunktfunkError, Result};
use aes_gcm::aead::{Aead, AeadInOut, KeyInit, Payload};
use aes_gcm::Aes128Gcm;
use chacha20poly1305::ChaCha20Poly1305;
use zeroize::Zeroize;

pub const TAG_LEN: usize = 16;

// CRYPTO_OVERHEAD and every in-place split assume both AEADs append TAG_LEN.
const _: () = assert!(std::mem::size_of::<aes_gcm::Tag>() == TAG_LEN);
const _: () = assert!(std::mem::size_of::<chacha20poly1305::Tag>() == TAG_LEN);

/// Negotiated AEAD plus matching key. Mixed cipher/key sizes are unrepresentable.
/// ChaCha is 32 bytes (RFC 8439); offered when the peer advertised
/// [`VIDEO_CAP_CHACHA20`](crate::quic::VIDEO_CAP_CHACHA20).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionKey {
    Aes128Gcm([u8; 16]),
    ChaCha20Poly1305([u8; 32]),
}

impl SessionKey {
    pub fn cipher_name(&self) -> &'static str {
        match self {
            SessionKey::Aes128Gcm(_) => "aes-128-gcm",
            SessionKey::ChaCha20Poly1305(_) => "chacha20-poly1305",
        }
    }

    /// All-zero key. `Config::validate` rejects this when encryption is on.
    pub fn is_zero(&self) -> bool {
        match self {
            SessionKey::Aes128Gcm(k) => k == &[0u8; 16],
            SessionKey::ChaCha20Poly1305(k) => k == &[0u8; 32],
        }
    }
}

/// Redacts key bytes; `Config`'s `Debug` depends on that.
impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionKey::Aes128Gcm(_) => f.write_str("Aes128Gcm(<redacted>)"),
            SessionKey::ChaCha20Poly1305(_) => f.write_str("ChaCha20Poly1305(<redacted>)"),
        }
    }
}

/// Zeroizes key material in place; `Config`'s `Drop` depends on that.
impl Zeroize for SessionKey {
    fn zeroize(&mut self) {
        match self {
            SessionKey::Aes128Gcm(k) => k.zeroize(),
            SessionKey::ChaCha20Poly1305(k) => k.zeroize(),
        }
    }
}

// One SessionCrypto per session; boxing would chase a pointer on every seal/open.
#[allow(clippy::large_enum_variant)]
enum Cipher {
    Aes128Gcm(Aes128Gcm),
    ChaCha20Poly1305(ChaCha20Poly1305),
}

pub struct SessionCrypto {
    cipher: Cipher,
    /// This side's nonce salt (direction bit set).
    send_salt: [u8; 4],
    /// Peer's nonce salt (the other direction bit).
    recv_salt: [u8; 4],
}

impl SessionCrypto {
    pub fn new(key: &SessionKey, salt: [u8; 4], role: Role) -> Self {
        let cipher = match key {
            // Compile-time `&[u8; N]` → `hybrid_array`; not runtime `from_slice`.
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

    /// Seal `plaintext` for `seq`. Returns `ciphertext || tag`. `seq` is AAD.
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

    /// Seal in place: `buf` is `[plaintext..][TAG_LEN scratch]`; returns
    /// `[ciphertext..][tag]`, byte-identical to [`seal`](Self::seal).
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

    /// Open `ciphertext || tag` for `seq` (also AAD).
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

    /// Open in place: `buf` is `[ciphertext..][tag]`; on success plaintext
    /// occupies the first `len - TAG_LEN` bytes (returned). Tag check runs
    /// before decrypt, so failure leaves `buf` as ciphertext.
    pub fn open_in_place(&self, seq: u64, buf: &mut [u8]) -> Result<usize> {
        if buf.len() < TAG_LEN {
            return Err(PunktfunkError::BadPacket);
        }
        let nonce = nonce(self.recv_salt, seq);
        let split = buf.len() - TAG_LEN;
        let (ciphertext, tag) = buf.split_at_mut(split);
        let aad = seq.to_be_bytes();
        // Tag is TAG_LEN (const-asserted). Map, don't unwrap: hot path must not panic.
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

/// Set `salt[0]`'s top bit to `dir` so the two directions never share a nonce.
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

/// Fresh AES-128 key for pairing / control-plane.
pub fn random_key() -> [u8; 16] {
    let mut k = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut k);
    k
}

/// Fresh 32-byte ChaCha20-Poly1305 key (RFC 8439).
pub fn random_key32() -> [u8; 32] {
    let mut k = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut k);
    k
}

pub fn random_salt() -> [u8; 4] {
    let mut s = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One fresh key per negotiated cipher; every sealing test below must hold for both.
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
            let sealed = host.seal(42, msg).unwrap();
            assert_ne!(&sealed[..msg.len()], &msg[..]);
            assert_eq!(sealed.len(), msg.len() + TAG_LEN);
            assert_eq!(client.open(42, &sealed).unwrap(), msg);

            assert!(client.open(43, &sealed).is_err());
            // Host open uses the peer salt, so it cannot open its own outbound packet.
            assert!(host.open(42, &sealed).is_err());
        }
    }

    #[test]
    fn directions_use_distinct_nonce_spaces() {
        for key in both_keys() {
            let salt = [0u8; 4]; // all-zero base salt must still separate the directions
            let host = SessionCrypto::new(&key, salt, Role::Host);
            let client = SessionCrypto::new(&key, salt, Role::Client);
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
                let mut buf = sealed.clone();
                assert!(client.open_in_place(8, &mut buf).is_err());
                let mut buf = sealed.clone();
                let last = buf.len() - 1;
                buf[last] ^= 1;
                assert!(client.open_in_place(9, &mut buf).is_err());
            }
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
                let reference = host.seal(7, msg).unwrap();
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
        // ChaCha key repeats the AES bytes so overlapping material still cannot interoperate.
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
        for key in both_keys() {
            let dbg = format!("{key:?}");
            assert!(dbg.contains("<redacted>"), "{dbg}");
        }
        let mut k = SessionKey::ChaCha20Poly1305([9u8; 32]);
        k.zeroize();
        assert!(k.is_zero());
    }
}
