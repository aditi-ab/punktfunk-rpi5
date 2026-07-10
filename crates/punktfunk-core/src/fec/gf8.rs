//! GF(2⁸) classic Reed–Solomon backend (vendored `fec-rs`). Uses the **Cauchy** generator
//! matrix `M[j][i] = inv[(m+i)^j]` over GF(2⁸) (poly 0x1d) — byte-identical to the `nanors`
//! library Moonlight uses, so the parity this produces is recoverable by a stock Moonlight
//! client (unlike Vandermonde RS, whose parity is not interoperable). Hard ceiling: data +
//! recovery ≤ 255 shards/block.

use super::{validate_block_shape, validate_encode_shape, ErasureCoder, FecError};
use crate::config::FecScheme;
use fec_rs::ReedSolomon;

pub struct Gf8Coder;

impl ErasureCoder for Gf8Coder {
    fn scheme(&self) -> FecScheme {
        FecScheme::Gf8
    }

    fn encode(&self, data: &[&[u8]], recovery_count: usize) -> Result<Vec<Vec<u8>>, FecError> {
        if recovery_count == 0 {
            return Ok(Vec::new());
        }
        validate_encode_shape(data)?;
        let k = data.len();
        let shard_len = data[0].len();
        let rs = ReedSolomon::new(k, recovery_count)
            .map_err(|_| FecError::Config("invalid GF(2^8) shard counts"))?;
        // `encode_sep` reads the data shards by reference and fills the parity in place —
        // same Cauchy codec as `encode`, without copying the data into a shards scratch.
        let mut parity: Vec<Vec<u8>> = (0..recovery_count).map(|_| vec![0u8; shard_len]).collect();
        rs.encode_sep(data, &mut parity)
            .map_err(|_| FecError::Backend("gf8 encode"))?;
        Ok(parity)
    }

    fn reconstruct(
        &self,
        data_count: usize,
        recovery_count: usize,
        received: &mut [Option<Vec<u8>>],
    ) -> Result<Vec<Vec<u8>>, FecError> {
        validate_block_shape(received, data_count, recovery_count)?;
        let present = received.iter().filter(|s| s.is_some()).count();
        if present < data_count {
            return Err(FecError::TooFewShards {
                have: present,
                need: data_count,
            });
        }
        if recovery_count == 0 {
            // No FEC: every original must already be present.
            return collect_originals(received, data_count);
        }
        let rs = ReedSolomon::new(data_count, recovery_count)
            .map_err(|_| FecError::Config("invalid GF(2^8) shard counts"))?;
        rs.reconstruct_data(received)
            .map_err(|_| FecError::Backend("gf8 reconstruct"))?;
        collect_originals(received, data_count)
    }
}

fn collect_originals(
    received: &[Option<Vec<u8>>],
    data_count: usize,
) -> Result<Vec<Vec<u8>>, FecError> {
    let mut out = Vec::with_capacity(data_count);
    for slot in received.iter().take(data_count) {
        out.push(
            slot.clone()
                .ok_or(FecError::Backend("reconstruction left an original missing"))?,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks byte-exact compatibility with Moonlight's `nanors` (Cauchy matrix
    /// `M[j][i] = inv[(m+i)^j]`, GF(2⁸) poly 0x1d). If the backend ever switched matrices,
    /// these vectors would break and our parity would no longer be Moonlight-decodable.
    #[test]
    fn nanors_exact_parity_vectors() {
        let coder = Gf8Coder;
        // The definitive nanors vector (k=4, m=2): single-byte shards [10,20,30,40] → [136, 0].
        let data: [&[u8]; 4] = [&[10u8], &[20], &[30], &[40]];
        let parity = coder.encode(&data, 2).unwrap();
        assert_eq!(parity, vec![vec![136u8], vec![0u8]]);

        // Cross-check independently from the Cauchy parity rows (proves the matrix, not just a
        // memorized output): parity[j] = XOR_i M[j][i] · data[i] over GF(2⁸).
        let rows = [[142u8, 244, 71, 167], [244, 142, 167, 71]];
        let din = [10u8, 20, 30, 40];
        for (j, row) in rows.iter().enumerate() {
            let expect = row
                .iter()
                .zip(din)
                .fold(0u8, |acc, (&m, d)| acc ^ gf_mul(m, d));
            assert_eq!(parity[j][0], expect, "parity row {j}");
        }
    }

    /// Round-trip: erase `m` data shards and confirm reconstruction recovers the originals.
    #[test]
    fn recovers_erased_data_shards() {
        let coder = Gf8Coder;
        let data: Vec<Vec<u8>> = (0..6).map(|i| vec![i as u8; 8]).collect();
        let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
        let parity = coder.encode(&refs, 3).unwrap();
        let mut received: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(parity.into_iter().map(Some))
            .collect();
        // Erase 3 data shards (the FEC budget) + nothing else.
        received[1] = None;
        received[3] = None;
        received[5] = None;
        let recovered = coder.reconstruct(6, 3, &mut received).unwrap();
        assert_eq!(recovered, data);
    }

    /// GF(2⁸) multiply, reduction poly 0x1d — independent of the backend.
    fn gf_mul(mut a: u8, mut b: u8) -> u8 {
        let mut p = 0u8;
        for _ in 0..8 {
            if b & 1 != 0 {
                p ^= a;
            }
            let hi = a & 0x80;
            a <<= 1;
            if hi != 0 {
                a ^= 0x1d;
            }
            b >>= 1;
        }
        p
    }
}
