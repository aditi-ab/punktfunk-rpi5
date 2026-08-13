//! Pairing crypto primitives (control plane only — distinct from `punktfunk_core`'s AES-GCM
//! data-plane sealing). GameStream pairing uses: AES-128-**ECB** with **no padding**,
//! SHA-256 (host appversion major ≥ 7), and RSA-PKCS1v15-SHA256 signatures. See the
//! `serverinfo + pairing` section of `design/research/gamestream-protocol-research.json`.

use aes::cipher::{Block, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use aes::Aes128;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// `n` cryptographically-random bytes.
pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rng().fill_bytes(&mut b);
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

/// Constant-time byte-slice equality — no early exit, so a timing side-channel can't probe the
/// expected value byte-by-byte. Returns false on a length mismatch (the length isn't secret here).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
    let cipher = Aes128::new(key.into());
    let mut out = data.to_vec();
    let rem = out.len() % 16;
    if rem != 0 {
        out.resize(out.len() + (16 - rem), 0);
    }
    for chunk in out.chunks_mut(16) {
        // The resize above made `out` a whole number of blocks, so every chunk is exactly 16.
        let block: &mut Block<Aes128> = chunk.try_into().expect("16-byte block");
        cipher.encrypt_block(block);
    }
    out
}

/// AES-128-ECB decrypt, no padding: trailing bytes past the last whole block are ignored.
pub fn ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(16) {
        // `chunks_exact(16)` yields only whole blocks; the short tail is dropped, as before.
        let mut block: Block<Aes128> = chunk.try_into().expect("16-byte block");
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}
