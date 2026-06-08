//! AES-128-GCM session sealing, matching GameStream's video crypto in P1.
//!
//! ## Nonce uniqueness (the GCM safety requirement)
//!
//! The 96-bit nonce is `salt (4 bytes) || sequence (8 bytes, big-endian)`. Reusing a
//! `(key, nonce)` pair under AES-GCM is catastrophic, so two precautions apply:
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

use crate::config::Role;
use crate::error::{LumenError, Result};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Key, Nonce};

/// 16-byte AEAD authentication tag appended by GCM.
pub const TAG_LEN: usize = 16;

pub struct SessionCrypto {
    cipher: Aes128Gcm,
    /// Salt for nonces we seal with (our direction).
    send_salt: [u8; 4],
    /// Salt for nonces we open with (the peer's direction).
    recv_salt: [u8; 4],
}

impl SessionCrypto {
    pub fn new(key: &[u8; 16], salt: [u8; 4], role: Role) -> Self {
        let key = Key::<Aes128Gcm>::from_slice(key);
        let own = direction(role);
        SessionCrypto {
            cipher: Aes128Gcm::new(key),
            send_salt: dir_salt(salt, own),
            recv_salt: dir_salt(salt, own ^ 1),
        }
    }

    /// Seal `plaintext` for sequence `seq`, returning `ciphertext || tag`. `seq` is
    /// authenticated as associated data.
    pub fn seal(&self, seq: u64, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = nonce(self.send_salt, seq);
        self.cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &seq.to_be_bytes(),
                },
            )
            .map_err(|_| LumenError::Crypto)
    }

    /// Open `ciphertext || tag` for sequence `seq` (also bound as associated data).
    pub fn open(&self, seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let nonce = nonce(self.recv_salt, seq);
        self.cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext,
                    aad: &seq.to_be_bytes(),
                },
            )
            .map_err(|_| LumenError::Crypto)
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

/// Generate a fresh random per-session nonce salt.
pub fn random_salt() -> [u8; 4] {
    let mut s = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_cross_direction() {
        let key = random_key();
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

    #[test]
    fn directions_use_distinct_nonce_spaces() {
        let key = random_key();
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
