//! The second seal lane (plan Phase 1.5): a persistent worker thread that AES-GCM-seals
//! the back half of a large frame's wire packets while the send thread seals the front.
//! [`Session`](super::Session)`::seal_frame_inner` owns the split policy; this module owns
//! the lane machinery and the shared per-slice seal loop.

use crate::crypto::SessionCrypto;
use crate::error::Result;

/// Wire-packet count at which a frame's sealing splits across two lanes (plan Phase 1.5):
/// below it the channel rendezvous (~µs) isn't worth it; at it the halved AES-GCM span
/// (≥ ~125 µs of ~1 µs/packet work) dwarfs the hand-off. ≈300 KB of wire, i.e. ≥150 Mbps
/// at 60 fps — small frames and the probe's ~17-packet AUs stay strictly single-lane.
pub(super) const TWO_LANE_MIN_PACKETS: usize = 256;

/// One two-lane seal hand-off: the frame's back-half wire buffers, sealed by the worker with
/// nonces `seq_base + i` (the nonce order is deterministic per shard index, which is what
/// makes the split sound). Round-trips through the channels so the buffers return to the pool.
pub(super) struct SealJob {
    pub(super) bufs: Vec<Vec<u8>>,
    pub(super) seq_base: u64,
    pub(super) timed: bool,
    /// Worker-lane CPU ns (when `timed`) and the seal outcome, filled in by the worker.
    pub(super) ns: u64,
    pub(super) result: Result<()>,
}

/// The persistent second seal lane: a worker thread that AES-GCM-seals the back half of a
/// large frame's packets while the send thread seals the front half. Rendezvous channels
/// (bound 1) — the send thread submits, seals its half, then waits; no per-frame spawn.
/// Dropping the struct closes the channel and the worker exits.
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

/// Seal a run of pre-written wire buffers in place: buffer `i` is `seq(8) ‖ plaintext ‖ tag
/// scratch` and seals over `[8..]` with sequence `seq_base + i` — the exact per-packet layout
/// and nonce order of the fused single-lane path. Shared by both lanes.
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
