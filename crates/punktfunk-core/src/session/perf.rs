//! `PUNKTFUNK_PERF` stage-timing telemetry for the two hot paths: where the client pump
//! and the host send thread actually spend their time, accumulated per report window and
//! drained via [`Session::take_pump_perf`](super::Session::take_pump_perf) /
//! [`Session::take_seal_perf`](super::Session::take_seal_perf).

use crate::fec::ErasureCoder;

/// Accumulated client receive-path stage timings since the last [`Session::take_pump_perf`](super::Session::take_pump_perf).
/// Answers "where does the pump core go" at line rate: kernel drain (`recv_ns`) vs AES-GCM
/// (`decrypt_ns`) vs reassembly+FEC (`reasm_ns`, the `Reassembler::push` round-trip including
/// shard copies and block reconstruction). 2026-07-14 sweep context: the pump pegs one core at
/// ~1.5 Gbps wire, ~85% of it userspace — this split is what Phase 2.1 (pooled reassembly) is
/// validated against.
#[derive(Debug, Default, Clone, Copy)]
pub struct PumpPerf {
    /// ns inside `recv_batch` (recvmmsg / recvmsg_x), i.e. syscall + kernel copy.
    pub recv_ns: u64,
    /// ns inside `open_in_place` across all datagrams (AES-128-GCM + replay-window upkeep).
    pub decrypt_ns: u64,
    /// ns inside `Reassembler::push` (header parse, shard copy, FEC reconstruct, AU assembly).
    pub reasm_ns: u64,
    /// recv_batch calls (batches) and datagrams processed over the accumulation window.
    pub batches: u64,
    pub packets: u64,
}

/// Accumulated host send-path stage timings since the last [`Session::take_seal_perf`](super::Session::take_seal_perf) (plan
/// Phase 0.4, host half). Answers "where does the send thread go" at rate: FEC parity
/// generation (`fec_ns`, inside [`ErasureCoder::encode_into`]) vs AES-GCM (`seal_ns`,
/// per-packet `seal_in_place`) vs the socket handoff (`sock_ns` — `send_gso`/`sendmmsg`
/// syscalls; the internal submit paths time it here, the paced video path folds its chunk
/// sends in via [`Session::note_sock_ns`](super::Session::note_sock_ns)). The Phase 1.5 gate reads off this split: build
/// two-lane seal only if `seal_ns` exceeds ~15% of the send thread at 2 Gbps.
#[derive(Debug, Default, Clone, Copy)]
pub struct SealPerf {
    /// ns inside `ErasureCoder::encode_into` (parity generation).
    pub fec_ns: u64,
    /// ns inside `seal_in_place` across all wire packets (AES-128-GCM).
    pub seal_ns: u64,
    /// ns inside `send_sealed` (socket syscalls), where the session can see it.
    pub sock_ns: u64,
    /// Frames sealed and wire packets sealed over the accumulation window.
    pub frames: u64,
    pub packets: u64,
}

/// [`ErasureCoder`] shim accumulating the time spent in `encode_into` (the send-path FEC
/// stage) — only constructed when `PUNKTFUNK_PERF` armed the session's [`SealPerf`]. The
/// counter is atomic purely to satisfy the trait's `Sync` bound; it lives on one thread.
pub(super) struct TimedCoder<'a> {
    pub(super) inner: &'a dyn ErasureCoder,
    pub(super) ns: &'a std::sync::atomic::AtomicU64,
}

impl ErasureCoder for TimedCoder<'_> {
    fn scheme(&self) -> crate::config::FecScheme {
        self.inner.scheme()
    }
    fn encode(
        &self,
        data: &[&[u8]],
        recovery_count: usize,
    ) -> std::result::Result<Vec<Vec<u8>>, crate::fec::FecError> {
        self.inner.encode(data, recovery_count)
    }
    fn encode_into(
        &self,
        data: &[&[u8]],
        recovery_count: usize,
        out: &mut Vec<Vec<u8>>,
    ) -> std::result::Result<(), crate::fec::FecError> {
        let t0 = std::time::Instant::now();
        let r = self.inner.encode_into(data, recovery_count, out);
        self.ns.fetch_add(
            t0.elapsed().as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        r
    }
    fn reconstruct(
        &self,
        data_count: usize,
        recovery_count: usize,
        received: &mut [Option<Vec<u8>>],
    ) -> std::result::Result<Vec<Vec<u8>>, crate::fec::FecError> {
        self.inner.reconstruct(data_count, recovery_count, received)
    }
    fn reconstruct_into(
        &self,
        recovery_count: usize,
        data: &mut [&mut [u8]],
        have: &[bool],
        recovery: &[(usize, &[u8])],
    ) -> std::result::Result<(), crate::fec::FecError> {
        self.inner
            .reconstruct_into(recovery_count, data, have, recovery)
    }
}
