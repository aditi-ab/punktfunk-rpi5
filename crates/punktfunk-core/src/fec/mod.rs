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

    /// [`encode`](Self::encode) into caller-pooled parity buffers: on success `out` holds
    /// exactly `recovery_count` shards, reusing its existing `Vec` allocations (extras are
    /// truncated away, missing ones are grown once to the high-water mark). The per-frame
    /// hot path (plan Phase 1.4) — backends also reuse their internal codec state here, so
    /// steady-state frames cost no encoder construction and no parity allocations. The
    /// default delegates to `encode` (correct, unpooled) for backends without an override.
    /// On error `out`'s contents are unspecified and must not be sent.
    fn encode_into(
        &self,
        data: &[&[u8]],
        recovery_count: usize,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<(), FecError> {
        *out = self.encode(data, recovery_count)?;
        Ok(())
    }

    /// Reconstruct the K original shards. `received` has length K+M: indices `0..K` are
    /// originals, `K..K+M` are recovery shards; `Some` = present, `None` = lost.
    /// Returns the K original shards in order.
    fn reconstruct(
        &self,
        data_count: usize,
        recovery_count: usize,
        received: &mut [Option<Vec<u8>>],
    ) -> Result<Vec<Vec<u8>>, FecError>;

    /// Reconstruct ONLY the missing data shards of a block, writing each straight into its final
    /// slot in the caller's buffer — the receive-side half of [`encode`](Self::encode)'s ref-based
    /// contract (the reassembler's slots are slices of one contiguous frame buffer, so recovery
    /// lands at its final AU offset with no per-shard `Vec`s and no block/AU concat copies).
    ///
    /// `data` holds the block's K equal-length shard slots; `have[i]` marks the slots whose bytes
    /// were received (valid codec input — a missing slot's contents are unspecified on entry).
    /// `recovery` is the received parity as `(recovery_index, bytes)` with `recovery_index <
    /// recovery_count` (the block's declared M, which the codec math needs even when not all M
    /// arrived). On success every missing slot has been filled; on error missing slots are
    /// unspecified and the caller must discard the block.
    fn reconstruct_into(
        &self,
        recovery_count: usize,
        data: &mut [&mut [u8]],
        have: &[bool],
        recovery: &[(usize, &[u8])],
    ) -> Result<(), FecError>;
}

/// Construct the coder for a scheme.
pub fn coder_for(scheme: FecScheme) -> Box<dyn ErasureCoder> {
    match scheme {
        FecScheme::Gf8 => Box::new(Gf8Coder::default()),
        FecScheme::Gf16 => Box::new(Gf16Coder::default()),
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

/// Validate the shape [`ErasureCoder::reconstruct_into`] promises: `have` matches `data`, one
/// shard length across data slots and recovery shards, recovery indices within the declared M,
/// and enough shards present to reconstruct at all. Both backends call this first.
pub(crate) fn validate_into_shape(
    data: &[&mut [u8]],
    have: &[bool],
    recovery: &[(usize, &[u8])],
    recovery_count: usize,
) -> Result<(), FecError> {
    if data.is_empty() {
        return Err(FecError::Config("no data shards"));
    }
    if have.len() != data.len() {
        return Err(FecError::Config("have length must equal data length"));
    }
    let len = data[0].len();
    if data.iter().any(|s| s.len() != len) {
        return Err(FecError::Config("shards in a block must be equal length"));
    }
    for &(j, bytes) in recovery {
        if j >= recovery_count {
            return Err(FecError::Config("recovery index out of range"));
        }
        if bytes.len() != len {
            return Err(FecError::Config("shards in a block must be equal length"));
        }
    }
    let present = have.iter().filter(|h| **h).count();
    if present + recovery.len() < data.len() {
        return Err(FecError::TooFewShards {
            have: present + recovery.len(),
            need: data.len(),
        });
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

    /// Round-trip through `reconstruct_into`: encode, zero out `lose_data` slots in a contiguous
    /// buffer (the reassembler's frame-buffer shape), drop `lose_recovery` parity shards, and
    /// assert the missing slots are restored in place while the present ones are untouched.
    fn roundtrip_into(
        coder: &dyn ErasureCoder,
        k: usize,
        m: usize,
        shard_len: usize,
        lose_data: &[usize],
        lose_recovery: &[usize],
    ) {
        let src: Vec<Vec<u8>> = (0..k)
            .map(|i| (0..shard_len).map(|b| (i * 31 + b * 7) as u8).collect())
            .collect();
        let refs: Vec<&[u8]> = src.iter().map(|s| s.as_slice()).collect();
        let parity = coder.encode(&refs, m).unwrap();

        let mut buf = vec![0u8; k * shard_len];
        let mut have = vec![true; k];
        for (i, s) in src.iter().enumerate() {
            if lose_data.contains(&i) {
                have[i] = false; // slot stays zeroed — codec must fill it
            } else {
                buf[i * shard_len..(i + 1) * shard_len].copy_from_slice(s);
            }
        }
        let recovery: Vec<(usize, &[u8])> = parity
            .iter()
            .enumerate()
            .filter(|(j, _)| !lose_recovery.contains(j))
            .map(|(j, p)| (j, p.as_slice()))
            .collect();

        let mut slots: Vec<&mut [u8]> = buf.chunks_mut(shard_len).collect();
        coder
            .reconstruct_into(m, &mut slots, &have, &recovery)
            .unwrap();
        for (i, s) in src.iter().enumerate() {
            assert_eq!(
                &buf[i * shard_len..(i + 1) * shard_len],
                s.as_slice(),
                "shard {i}"
            );
        }
    }

    #[test]
    fn gf16_reconstruct_into_fills_only_the_holes() {
        roundtrip_into(&Gf16Coder::default(), 16, 4, 256, &[1, 9], &[3]);
        roundtrip_into(&Gf16Coder::default(), 4, 2, 16, &[0, 3], &[]);
        roundtrip_into(&Gf16Coder::default(), 4, 2, 16, &[], &[0, 1]); // nothing missing, no parity needed
    }

    #[test]
    fn gf8_reconstruct_into_fills_only_the_holes() {
        roundtrip_into(&Gf8Coder::default(), 16, 4, 256, &[0, 7], &[1]);
        roundtrip_into(&Gf8Coder::default(), 4, 2, 16, &[2], &[1]);
    }

    #[test]
    fn reconstruct_into_rejects_bad_shapes() {
        let mut buf = [0u8; 4 * 8];
        // Too few shards: 2 of 4 data present, no recovery.
        let mut slots: Vec<&mut [u8]> = buf.chunks_mut(8).collect();
        let have = [true, true, false, false];
        assert!(Gf16Coder::default()
            .reconstruct_into(2, &mut slots, &have, &[])
            .is_err());
        // Recovery index out of the declared range.
        let parity = [0u8; 8];
        let mut slots: Vec<&mut [u8]> = buf.chunks_mut(8).collect();
        assert!(Gf16Coder::default()
            .reconstruct_into(2, &mut slots, &have, &[(2, &parity), (3, &parity)])
            .is_err());
        // Mismatched recovery shard length.
        let short = [0u8; 6];
        let mut slots: Vec<&mut [u8]> = buf.chunks_mut(8).collect();
        assert!(Gf8Coder::default()
            .reconstruct_into(2, &mut slots, &have, &[(0, &short), (1, &parity)])
            .is_err());
        // `have` length disagreeing with `data`.
        let mut slots: Vec<&mut [u8]> = buf.chunks_mut(8).collect();
        assert!(Gf8Coder::default()
            .reconstruct_into(2, &mut slots, &[true; 3], &[(0, &parity)])
            .is_err());
    }

    #[test]
    fn gf8_recovers_within_budget() {
        // 16 data + 4 recovery; lose 2 data + 2 recovery (== budget).
        roundtrip(&Gf8Coder::default(), 16, 4, 256, &[0, 7, 16, 19]);
    }

    #[test]
    fn gf16_recovers_within_budget() {
        roundtrip(&Gf16Coder::default(), 16, 4, 256, &[1, 9, 17, 18]);
    }

    #[test]
    fn gf8_too_much_loss_errors() {
        let data: Vec<Vec<u8>> = (0..8).map(|_| vec![0u8; 64]).collect();
        let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
        let recovery = Gf8Coder::default().encode(&refs, 2).unwrap();
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
        assert!(Gf16Coder::default().scheme() == FecScheme::Gf16);
        let err = Gf8Coder::default().reconstruct(8, 2, &mut received);
        assert!(err.is_err());
    }

    #[test]
    fn reconstruct_rejects_wrong_received_length() {
        // data=2, recovery=2 expects a 4-element slice; a 3-element one must error, not
        // panic on the recovery-slice index (both backends).
        let mut recv: Vec<Option<Vec<u8>>> = vec![Some(vec![0u8; 8]), None, Some(vec![0u8; 8])];
        assert!(Gf16Coder::default().reconstruct(2, 2, &mut recv).is_err());
        let mut recv: Vec<Option<Vec<u8>>> = vec![Some(vec![0u8; 8]), None, Some(vec![0u8; 8])];
        assert!(Gf8Coder::default().reconstruct(2, 2, &mut recv).is_err());
    }

    #[test]
    fn reconstruct_rejects_mismatched_shard_lengths() {
        // The GF16 fast path used to clone shards verbatim without a length check.
        let mut recv: Vec<Option<Vec<u8>>> =
            vec![Some(vec![0u8; 8]), Some(vec![0u8; 6]), None, None];
        assert!(Gf16Coder::default().reconstruct(2, 2, &mut recv).is_err());
        let mut recv: Vec<Option<Vec<u8>>> =
            vec![Some(vec![0u8; 8]), Some(vec![0u8; 6]), None, None];
        assert!(Gf8Coder::default().reconstruct(2, 2, &mut recv).is_err());
    }
}
