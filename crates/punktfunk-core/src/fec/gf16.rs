//! GF(2¹⁶) Leopard-RS backend (`reed-solomon-simd`). SIMD, O(n log n), up to 65535
//! shards/block — this is what removes the GameStream 255-shard / ~1 Gbps wall.
//! Shard length must be even.

use super::{validate_block_shape, validate_encode_shape, ErasureCoder, FecError};
use crate::config::FecScheme;

pub struct Gf16Coder;

impl ErasureCoder for Gf16Coder {
    fn scheme(&self) -> FecScheme {
        FecScheme::Gf16
    }

    fn encode(&self, data: &[&[u8]], recovery_count: usize) -> Result<Vec<Vec<u8>>, FecError> {
        if recovery_count == 0 {
            return Ok(Vec::new());
        }
        validate_encode_shape(data)?;
        let k = data.len();
        if data[0].len() % 2 != 0 {
            return Err(FecError::Config("GF(2^16) shard length must be even"));
        }
        reed_solomon_simd::encode(k, recovery_count, data)
            .map_err(|_| FecError::Backend("gf16 encode"))
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
        // Fast path: all originals already present, or FEC disabled.
        let originals_complete = received[..data_count].iter().all(|s| s.is_some());
        if recovery_count == 0 || originals_complete {
            let mut out = Vec::with_capacity(data_count);
            for slot in received.iter().take(data_count) {
                out.push(slot.clone().ok_or(FecError::TooFewShards {
                    have: present,
                    need: data_count,
                })?);
            }
            return Ok(out);
        }

        // Hand the decoder the surviving originals and recovery shards, indexed.
        let original_in: Vec<(usize, &[u8])> = received[..data_count]
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_deref().map(|b| (i, b)))
            .collect();
        let recovery_in: Vec<(usize, &[u8])> = received[data_count..data_count + recovery_count]
            .iter()
            .enumerate()
            .filter_map(|(j, s)| s.as_deref().map(|b| (j, b)))
            .collect();

        let restored =
            reed_solomon_simd::decode(data_count, recovery_count, original_in, recovery_in)
                .map_err(|_| FecError::Backend("gf16 decode"))?;

        // Merge surviving originals with the recovered ones.
        let mut out: Vec<Vec<u8>> = Vec::with_capacity(data_count);
        for (i, slot) in received[..data_count].iter().enumerate() {
            if let Some(s) = slot {
                out.push(s.clone());
            } else if let Some(s) = restored.get(&i) {
                out.push(s.clone());
            } else {
                return Err(FecError::Backend("gf16 decode left an original missing"));
            }
        }
        Ok(out)
    }
}
