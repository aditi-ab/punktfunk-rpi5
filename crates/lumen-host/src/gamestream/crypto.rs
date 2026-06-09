//! Pairing crypto primitives (control plane only — distinct from `lumen_core`'s AES-GCM
//! data-plane sealing). GameStream pairing uses: AES-128-**ECB** with **no padding**,
//! SHA-256 (host appversion major ≥ 7), and RSA-PKCS1v15-SHA256 signatures. See the
//! `serverinfo + pairing` section of `docs/research/gamestream-protocol-research.json`.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// `n` cryptographically-random bytes.
pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

/// SHA-256 over the concatenation of `parts`.
pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// The PIN-derived AES-128 key: `SHA-256(salt || pin)[..16]` (salt first, PIN as ASCII).
pub fn pin_key(salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let d = sha256(&[salt, pin.as_bytes()]);
    let mut k = [0u8; 16];
    k.copy_from_slice(&d[..16]);
    k
}

/// AES-128-ECB encrypt, no padding: input is zero-extended to a 16-byte multiple.
pub fn ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = data.to_vec();
    let rem = out.len() % 16;
    if rem != 0 {
        out.resize(out.len() + (16 - rem), 0);
    }
    for chunk in out.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }
    out
}

/// AES-128-ECB decrypt, no padding: trailing bytes past the last whole block are ignored.
pub fn ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        let mut block = *GenericArray::from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}
