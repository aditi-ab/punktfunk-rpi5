//! Persistent worker that AES-GCM-seals the back half of a large frame while the
//! send thread seals the front. [`Session`](super::Session) owns the split;
//! this module owns the lane and [`seal_wire_slice`]. Byte-identical to a
//! sequential pass; pinned by `zero_copy_seal_matches_wrapper_path`.

use crate::crypto::SessionCrypto;
use crate::error::Result;

/// Rendezvous is ~µs; AES-GCM is ~1 µs/packet. At 256 packets the halved span
/// (≥ ~125 µs) dwarfs the hand-off. ≈300 KB of wire, ≥150 Mbps at 60 fps.
pub(super) const TWO_LANE_MIN_PACKETS: usize = 256;

/// Round-trips through the channels so the buffers return to the pool.
pub(super) struct SealJob {
    pub(super) bufs: Vec<Vec<u8>>,
    pub(super) seq_base: u64,
    pub(super) timed: bool,
    pub(super) ns: u64,
    pub(super) result: Result<()>,
}

/// Bound-1 rendezvous, not a per-frame spawn. Drop closes the channel and the
/// worker exits.
pub(super) struct SealLane {
    pub(super) to_worker: std::sync::mpsc::SyncSender<SealJob>,
    pub(super) from_worker: std::sync::mpsc::Receiver<SealJob>,
}

impl SealLane {
    pub(super) fn spawn(crypto: std::sync::Arc<SessionCrypto>) -> Option<SealLane> {
        let (to_worker, jobs) = std::sync::mpsc::sync_channel::<SealJob>(1);
        let (done_tx, from_worker) = std::sync::mpsc::sync_channel::<SealJob>(1);
        std::thread::Builder::new()
            .name("punktfunk-seal2".into())
            .spawn(move || {
                while let Ok(mut job) = jobs.recv() {
                    let t0 = job.timed.then(std::time::Instant::now);
                    job.result = seal_wire_slice(&crypto, &mut job.bufs, job.seq_base);
                    if let Some(t0) = t0 {
                        job.ns = t0.elapsed().as_nanos() as u64;
                    }
                    if done_tx.send(job).is_err() {
                        break; // session gone mid-frame — nothing left to seal for
                    }
                }
            })
            .ok()?;
        Some(SealLane {
            to_worker,
            from_worker,
        })
    }
}

/// Buffer `i` is `seq(8) ‖ plaintext ‖ tag scratch`; seals `[8..]` under nonce
/// `seq_base + i`. Same layout and nonce order as the fused single-lane path.
pub(super) fn seal_wire_slice(
    c: &SessionCrypto,
    wires: &mut [Vec<u8>],
    seq_base: u64,
) -> Result<()> {
    for (i, wire) in wires.iter_mut().enumerate() {
        c.seal_in_place(seq_base.wrapping_add(i as u64), &mut wire[8..])?;
    }
    Ok(())
}
