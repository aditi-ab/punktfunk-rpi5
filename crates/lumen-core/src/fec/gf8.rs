//! GF(2⁸) classic Reed–Solomon backend (`reed-solomon-erasure`), equivalent to the
//! `nanors` library Moonlight uses. Hard ceiling: data + recovery ≤ 255 shards/block.

use super::{validate_block_shape, validate_encode_shape, ErasureCoder, FecError};
use crate::config::FecScheme;
use reed_solomon_erasure::galois_8::ReedSolomon;

pub struct Gf8Coder;

impl ErasureCoder for Gf8Coder {
    fn scheme(&self) -> FecScheme {
        FecScheme::Gf8
    }

    fn encode(&self, data: &[Vec<u8>], recovery_count: usize) -> Result<Vec<Vec<u8>>, FecError> {
        if recovery_count == 0 {
            return Ok(Vec::new());
        }
        validate_encode_shape(data)?;
        let k = data.len();
        let shard_len = data[0].len();
        let rs = ReedSolomon::new(k, recovery_count)
            .map_err(|_| FecError::Config("invalid GF(2^8) shard counts"))?;
        // reed-solomon-erasure fills parity in place: shards = data || zeroed parity.
        let mut shards: Vec<Vec<u8>> = Vec::with_capacity(k + recovery_count);
        shards.extend_from_slice(data);
        shards.resize_with(k + recovery_count, || vec![0u8; shard_len]);
        rs.encode(&mut shards)
            .map_err(|_| FecError::Backend("gf8 encode"))?;
        Ok(shards.split_off(k))
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
