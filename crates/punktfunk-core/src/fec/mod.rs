//! Erasure coding. Two backends behind one [`ErasureCoder`] trait: GF(2⁸) (classic
//! Reed–Solomon, Moonlight-compatible, P1) and GF(2¹⁶) Leopard-RS (the wall-breaker, P2).
//!
//! The wall this breaks: GameStream's GF(2⁸) RS caps a block at 255 shards, which at
//! 5120×1440@240 is hit around 1 Gbps. GF(2¹⁶) raises that ceiling to 65535 shards and
//! runs in O(n log n) with SIMD, so the per-frame shard count stops being the limiter.

mod gf16;
mod gf8;

pub use gf16::Gf16Coder;
pub use gf8::Gf8Coder;

use crate::config::FecScheme;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FecError {
    #[error("invalid shard configuration: {0}")]
    Config(&'static str),
    #[error("too few shards to reconstruct (have {have}, need {need})")]
    TooFewShards { have: usize, need: usize },
    #[error("backend error: {0}")]
    Backend(&'static str),
}

/// Backend-agnostic erasure coder. All shards in a block are equal length.
pub trait ErasureCoder: Send + Sync {
    fn scheme(&self) -> FecScheme;

    /// Encode `data` (K original shards) into `recovery_count` (M) parity shards.
    /// Returns the M recovery shards. `recovery_count == 0` returns an empty `Vec`.
    /// Takes shard *references* so the packetizer can point straight into the frame
    /// buffer instead of copying every data byte into per-shard `Vec`s first.
    fn encode(&self, data: &[&[u8]], recovery_count: usize) -> Result<Vec<Vec<u8>>, FecError>;

    /// Reconstruct the K original shards. `received` has length K+M: indices `0..K` are
    /// originals, `K..K+M` are recovery shards; `Some` = present, `None` = lost.
    /// Returns the K original shards in order.
    fn reconstruct(
        &self,
        data_count: usize,
        recovery_count: usize,
        received: &mut [Option<Vec<u8>>],
    ) -> Result<Vec<Vec<u8>>, FecError>;
}

/// Construct the coder for a scheme.
pub fn coder_for(scheme: FecScheme) -> Box<dyn ErasureCoder> {
    match scheme {
        FecScheme::Gf8 => Box::new(Gf8Coder),
        FecScheme::Gf16 => Box::new(Gf16Coder),
    }
}

/// Validate the shape `reconstruct` promises: `received.len() == data + recovery`, and
/// every present shard shares one length. Both backends call this first so neither the
/// fast path nor a malformed caller can slip mismatched-length or wrong-count shards
/// through (the fast paths bypass the backend's own length checks otherwise).
pub(crate) fn validate_block_shape(
    received: &[Option<Vec<u8>>],
    data_count: usize,
    recovery_count: usize,
) -> Result<(), FecError> {
    if received.len() != data_count + recovery_count {
        return Err(FecError::Config(
            "received length must equal data + recovery",
        ));
    }
    let mut len = None;
    for s in received.iter().flatten() {
        match len {
            None => len = Some(s.len()),
            Some(l) if l != s.len() => {
                return Err(FecError::Config("shards in a block must be equal length"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validate `encode` inputs: at least one data shard, all of equal length.
pub(crate) fn validate_encode_shape(data: &[&[u8]]) -> Result<(), FecError> {
    let first = data
        .first()
        .ok_or(FecError::Config("no data shards"))?
        .len();
    if data.iter().any(|s| s.len() != first) {
        return Err(FecError::Config("data shards must be equal length"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a block through a coder, losing exactly `lose` shards (some data,
    /// some recovery), and assert the originals come back byte-identical.
    fn roundtrip(coder: &dyn ErasureCoder, k: usize, m: usize, shard_len: usize, lose: &[usize]) {
        let data: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..shard_len).map(|b| (i * 31 + b * 7) as u8).collect())
            .collect();
        let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
        let recovery = coder.encode(&refs, m).unwrap();
        assert_eq!(recovery.len(), m);

        let mut received: Vec<Option<Vec<u8>>> = Vec::with_capacity(k + m);
        received.extend(data.iter().cloned().map(Some));
        received.extend(recovery.iter().cloned().map(Some));
        for &idx in lose {
            received[idx] = None;
        }

        let restored = coder.reconstruct(k, m, &mut received).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn gf8_recovers_within_budget() {
        // 16 data + 4 recovery; lose 2 data + 2 recovery (== budget).
        roundtrip(&Gf8Coder, 16, 4, 256, &[0, 7, 16, 19]);
    }

    #[test]
    fn gf16_recovers_within_budget() {
        roundtrip(&Gf16Coder, 16, 4, 256, &[1, 9, 17, 18]);
    }

    #[test]
    fn gf8_too_much_loss_errors() {
        let data: Vec<Vec<u8>> = (0..8).map(|_| vec![0u8; 64]).collect();
        let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
        let recovery = Gf8Coder.encode(&refs, 2).unwrap();
        let mut received: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .map(Some)
            .chain(recovery.into_iter().map(Some))
            .collect();
        // Lose 3 with only 2 recovery shards → unrecoverable.
        received[0] = None;
        received[1] = None;
        received[2] = None;
        assert!(Gf16Coder.scheme() == FecScheme::Gf16);
        let err = Gf8Coder.reconstruct(8, 2, &mut received);
        assert!(err.is_err());
    }

    #[test]
    fn reconstruct_rejects_wrong_received_length() {
        // data=2, recovery=2 expects a 4-element slice; a 3-element one must error, not
        // panic on the recovery-slice index (both backends).
        let mut recv: Vec<Option<Vec<u8>>> = vec![Some(vec![0u8; 8]), None, Some(vec![0u8; 8])];
        assert!(Gf16Coder.reconstruct(2, 2, &mut recv).is_err());
        let mut recv: Vec<Option<Vec<u8>>> = vec![Some(vec![0u8; 8]), None, Some(vec![0u8; 8])];
        assert!(Gf8Coder.reconstruct(2, 2, &mut recv).is_err());
    }

    #[test]
    fn reconstruct_rejects_mismatched_shard_lengths() {
        // The GF16 fast path used to clone shards verbatim without a length check.
        let mut recv: Vec<Option<Vec<u8>>> =
            vec![Some(vec![0u8; 8]), Some(vec![0u8; 6]), None, None];
        assert!(Gf16Coder.reconstruct(2, 2, &mut recv).is_err());
        let mut recv: Vec<Option<Vec<u8>>> =
            vec![Some(vec![0u8; 8]), Some(vec![0u8; 6]), None, None];
        assert!(Gf8Coder.reconstruct(2, 2, &mut recv).is_err());
    }
}
