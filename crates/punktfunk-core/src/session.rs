//! Session lifecycle and the two hot-path state machines.
//!
//! - **Host** ([`Session::submit_frame`]): encoded access unit → FEC + packetize →
//!   optional AES-GCM seal → transport send.
//! - **Client** ([`Session::poll_frame`]): transport recv → optional open → reorder +
//!   FEC recover + reassemble → whole access unit.
//!
//! Both directions also carry input: a client [`Session::send_input`]s events; the host
//! drains them with [`Session::poll_input`].

use crate::config::{Config, Role};
use crate::crypto::SessionCrypto;
use crate::error::{PunktfunkError, Result};
use crate::fec::{coder_for, ErasureCoder};
use crate::input::InputEvent;
use crate::packet::{Packetizer, Reassembler, ReassemblerLimits, MAX_DATAGRAM_BYTES};
use crate::stats::{Stats, StatsCounters};
use crate::transport::Transport;
use zerocopy::IntoBytes;

/// A reassembled, FEC-recovered access unit, ready to hand to the platform decoder.
pub struct Frame {
    pub data: Vec<u8>,
    pub frame_index: u32,
    pub pts_ns: u64,
    pub flags: u32,
}

/// One end of a stream. Constructed for a single [`Role`]; calling the other role's
/// methods returns [`PunktfunkError::InvalidArg`].
///
/// Anti-replay: the receive path runs each opened datagram's AEAD-authenticated sequence through a
/// sliding-window filter ([`ReplayWindow`]), so a captured, validly-sealed datagram can't be replayed
/// by an on-path attacker — closing the input-replay gap that previously rested solely on the
/// LAN/VPN transport assumption (plan §1). Genuine reordering within the window is still accepted;
/// video additionally benefits from the reassembler's per-frame dedup.
pub struct Session {
    config: Config,
    coder: Box<dyn ErasureCoder>,
    /// `Arc` so the second seal lane (Phase 1.5) can share the cipher; uncontended otherwise.
    crypto: Option<std::sync::Arc<SessionCrypto>>,
    /// Anti-replay window over the peer's authenticated sequence (receive side). `Some` exactly when
    /// `crypto` is — the plaintext probe path carries no sequence to filter on.
    replay: Option<ReplayWindow>,
    transport: Box<dyn Transport>,
    packetizer: Packetizer,
    reassembler: Reassembler,
    stats: StatsCounters,
    /// Monotonic wire sequence, also the AES-GCM nonce counter.
    next_seq: u64,
    /// Client recv ring (reused across [`poll_frame`](Self::poll_frame)): `recvmmsg` drains a batch
    /// of datagrams into `recv_scratch` in one syscall, and poll_frame consumes them one at a time
    /// across calls (`recv_idx`..`recv_count`), refilling when drained. Allocated lazily on the
    /// first client poll so host sessions don't carry it. No per-packet recv alloc at line rate.
    recv_scratch: Vec<Vec<u8>>,
    recv_lens: Vec<usize>,
    recv_count: usize,
    recv_idx: usize,
    /// Host send pool: reused wire buffers (`seal_frame` seals in place into these, the caller sends
    /// then returns them via [`reclaim_wires`](Self::reclaim_wires)). After warmup each buffer keeps
    /// its capacity, so the per-packet ciphertext + wire `Vec` allocations vanish from the hot path.
    wire_pool: Vec<Vec<u8>>,
    /// Receive-path stage timing (`PUNKTFUNK_PERF`), read+reset via [`take_pump_perf`]
    /// (Self::take_pump_perf). `None` when disabled — the hot path then pays one branch per stage.
    perf: Option<PumpPerf>,
    /// Send-path stage timing (`PUNKTFUNK_PERF`), read+reset via [`take_seal_perf`]
    /// (Self::take_seal_perf). Same arming + branch-cost contract as `perf`.
    seal_perf: Option<SealPerf>,
    /// The second seal lane (plan Phase 1.5), lazily spawned by the first frame that crosses
    /// [`TWO_LANE_MIN_PACKETS`]. Host sessions only (client sessions never seal frames).
    seal_lane: Option<SealLane>,
    /// Two-lane sealing enabled (default). `PUNKTFUNK_SEAL_LANES=1` forces single-lane.
    seal_two_lane: bool,
    /// Reused header-Vec for the lane hand-off (the worker's half round-trips through this,
    /// so steady-state two-lane frames move `n/2` Vec headers with zero allocation).
    lane_scratch: Vec<Vec<u8>>,
}

/// Wire-packet count at which a frame's sealing splits across two lanes (plan Phase 1.5):
/// below it the channel rendezvous (~µs) isn't worth it; at it the halved AES-GCM span
/// (≥ ~125 µs of ~1 µs/packet work) dwarfs the hand-off. ≈300 KB of wire, i.e. ≥150 Mbps
/// at 60 fps — small frames and the probe's ~17-packet AUs stay strictly single-lane.
const TWO_LANE_MIN_PACKETS: usize = 256;

/// One two-lane seal hand-off: the frame's back-half wire buffers, sealed by the worker with
/// nonces `seq_base + i` (the nonce order is deterministic per shard index, which is what
/// makes the split sound). Round-trips through the channels so the buffers return to the pool.
struct SealJob {
    bufs: Vec<Vec<u8>>,
    seq_base: u64,
    timed: bool,
    /// Worker-lane CPU ns (when `timed`) and the seal outcome, filled in by the worker.
    ns: u64,
    result: Result<()>,
}

/// The persistent second seal lane: a worker thread that AES-GCM-seals the back half of a
/// large frame's packets while the send thread seals the front half. Rendezvous channels
/// (bound 1) — the send thread submits, seals its half, then waits; no per-frame spawn.
/// Dropping the struct closes the channel and the worker exits.
struct SealLane {
    to_worker: std::sync::mpsc::SyncSender<SealJob>,
    from_worker: std::sync::mpsc::Receiver<SealJob>,
}

impl SealLane {
    fn spawn(crypto: std::sync::Arc<SessionCrypto>) -> Option<SealLane> {
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
fn seal_wire_slice(c: &SessionCrypto, wires: &mut [Vec<u8>], seq_base: u64) -> Result<()> {
    for (i, wire) in wires.iter_mut().enumerate() {
        c.seal_in_place(seq_base.wrapping_add(i as u64), &mut wire[8..])?;
    }
    Ok(())
}

/// Accumulated client receive-path stage timings since the last [`Session::take_pump_perf`].
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

/// Accumulated host send-path stage timings since the last [`Session::take_seal_perf`] (plan
/// Phase 0.4, host half). Answers "where does the send thread go" at rate: FEC parity
/// generation (`fec_ns`, inside [`ErasureCoder::encode_into`]) vs AES-GCM (`seal_ns`,
/// per-packet `seal_in_place`) vs the socket handoff (`sock_ns` — `send_gso`/`sendmmsg`
/// syscalls; the internal submit paths time it here, the paced video path folds its chunk
/// sends in via [`Session::note_sock_ns`]). The Phase 1.5 gate reads off this split: build
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
struct TimedCoder<'a> {
    inner: &'a dyn ErasureCoder,
    ns: &'a std::sync::atomic::AtomicU64,
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

/// Datagrams drained per `recvmmsg` syscall on the client (the reused ring's size). 128 keeps
/// the syscall rate ≤ ~3.4k/s even at the ~430k pkt/s the post-2026-07-14 receive path delivers
/// (~4.8 Gbps wire), and gives the kernel buffer a deeper drain per pump iteration; the buffers
/// cost `RECV_BATCH × RECV_BUF` (~256 KB, client sessions only).
const RECV_BATCH: usize = 128;

impl Session {
    pub fn new(config: Config, transport: Box<dyn Transport>) -> Result<Session> {
        config.validate()?;
        let coder = coder_for(config.fec.scheme);
        let crypto = config.encrypt.then(|| {
            std::sync::Arc::new(SessionCrypto::new(&config.key, config.salt, config.role))
        });
        // A receive-side replay window exists exactly when the datagrams are sealed (they carry the
        // authenticated sequence the window keys on). Both roles receive from their peer.
        let replay = config.encrypt.then(ReplayWindow::new);
        let packetizer = Packetizer::new(&config);
        let reassembler = Reassembler::new(ReassemblerLimits::from_config(&config));
        Ok(Session {
            coder,
            crypto,
            replay,
            transport,
            packetizer,
            reassembler,
            stats: StatsCounters::default(),
            next_seq: 0,
            recv_scratch: Vec::new(),
            recv_lens: Vec::new(),
            recv_count: 0,
            recv_idx: 0,
            wire_pool: Vec::new(),
            // Same opt-in the host's stage logs use; read once — set it before connecting.
            perf: std::env::var("PUNKTFUNK_PERF")
                .is_ok_and(|v| v != "0")
                .then(PumpPerf::default),
            seal_perf: std::env::var("PUNKTFUNK_PERF")
                .is_ok_and(|v| v != "0")
                .then(SealPerf::default),
            seal_lane: None,
            // Two-lane sealing of large frames is the default; =1 forces single-lane (the
            // escape hatch — behavior is byte-identical, this only changes who seals).
            seal_two_lane: std::env::var("PUNKTFUNK_SEAL_LANES")
                .map(|v| v != "1")
                .unwrap_or(true),
            lane_scratch: Vec::new(),
            config,
        })
    }

    /// Drain the receive-path stage timings accumulated since the last call (window semantics —
    /// the pump reads this once per report interval). `None` when `PUNKTFUNK_PERF` is off.
    pub fn take_pump_perf(&mut self) -> Option<PumpPerf> {
        self.perf.as_mut().map(std::mem::take)
    }

    /// Drain the send-path stage timings accumulated since the last call (window semantics —
    /// the host send loop reads this once per perf window). `None` when `PUNKTFUNK_PERF` is off.
    pub fn take_seal_perf(&mut self) -> Option<SealPerf> {
        self.seal_perf.as_mut().map(std::mem::take)
    }

    /// Fold externally-timed socket time into [`SealPerf::sock_ns`] — the paced video path
    /// times its own `send_sealed` chunk calls (they happen behind a `&self` borrow inside the
    /// pacing closure, where the session can't self-time). No-op when perf is off.
    pub fn note_sock_ns(&mut self, ns: u64) {
        if let Some(p) = self.seal_perf.as_mut() {
            p.sock_ns += ns;
        }
    }

    pub fn role(&self) -> Role {
        self.config.role
    }

    pub fn stats(&self) -> Stats {
        self.stats.snapshot()
    }

    /// Wrap a packet for the wire: when encrypting, prepend the 8-byte big-endian
    /// sequence (the receiver derives the GCM nonce from it) then the ciphertext.
    /// Seal one plaintext packet into the reused `wire` buffer in place (no allocation): the wire is
    /// `seq(8) || ciphertext || tag` with crypto on, or just the packet with crypto off (probe).
    /// Byte-identical to the previous `seal` + concat path; `clear()` keeps the buffer's capacity.
    fn seal_into(&mut self, packet: &[u8], wire: &mut Vec<u8>) -> Result<()> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        wire.clear();
        match &self.crypto {
            Some(c) => {
                wire.extend_from_slice(&seq.to_be_bytes()); // [0..8] plaintext seq prefix
                wire.extend_from_slice(packet); // [8..8+n] plaintext to encrypt
                wire.resize(wire.len() + crate::crypto::TAG_LEN, 0); // tag scratch
                c.seal_in_place(seq, &mut wire[8..])?; // encrypt [8..] in place, tag written at the end
            }
            None => wire.extend_from_slice(packet),
        }
        Ok(())
    }

    /// Unwrap a wire datagram back into a plaintext packet.
    fn open_from_wire(&self, wire: &[u8]) -> Result<Vec<u8>> {
        match &self.crypto {
            Some(c) => {
                if wire.len() < 8 {
                    return Err(PunktfunkError::BadPacket);
                }
                let seq = u64::from_be_bytes(wire[..8].try_into().unwrap());
                c.open(seq, &wire[8..])
            }
            None => Ok(wire.to_vec()),
        }
    }

    /// Feed an opened datagram's authenticated sequence to the anti-replay window: `true` = fresh
    /// (accept), `false` = a replay or older than the window (drop). Returns `true` when the session
    /// isn't encrypting (no window, and no sequence on the wire to key on).
    fn accept_seq(&mut self, seq: u64) -> bool {
        match self.replay.as_mut() {
            Some(w) => w.accept(seq),
            None => true,
        }
    }

    // -- Host path --------------------------------------------------------

    /// Host: FEC-protect, packetize, and seal one encoded access unit into wire packets WITHOUT
    /// sending them. Counts the frame + its packets/bytes as submitted; the caller transmits the
    /// returned packets via [`send_sealed`](Self::send_sealed) — in one call, or in chunks paced
    /// over the frame interval so a real NIC doesn't drop the whole frame as a line-rate burst (the
    /// 1 Gbps+ freeze fix). The nonce counter advances per packet, in order, so seal once and send
    /// the result intact. (Holding the `Vec<Vec<u8>>` also keeps the buffers alive for the batch.)
    pub fn seal_frame(
        &mut self,
        data: &[u8],
        pts_ns: u64,
        user_flags: u32,
    ) -> Result<Vec<Vec<u8>>> {
        self.seal_frame_inner(data, pts_ns, user_flags, None)
    }

    /// [`seal_frame`](Self::seal_frame) with the caller's **explicit** `frame_index` instead of the
    /// packetizer's internal counter. The punktfunk/1 encode loop owns the video numbering (one
    /// session-lifetime counter, stamped per AU) so the encoder's reference-frame-invalidation
    /// bookkeeping stays 1:1 with the wire across encoder rebuilds/resets — see
    /// [`Packetizer::packetize_each`]. A session must use ONE numbering style per index space.
    pub fn seal_frame_at(
        &mut self,
        data: &[u8],
        pts_ns: u64,
        user_flags: u32,
        frame_index: u32,
    ) -> Result<Vec<Vec<u8>>> {
        self.seal_frame_inner(data, pts_ns, user_flags, Some(frame_index))
    }

    fn seal_frame_inner(
        &mut self,
        data: &[u8],
        pts_ns: u64,
        user_flags: u32,
        frame_index: Option<u32>,
    ) -> Result<Vec<Vec<u8>>> {
        if self.config.role != Role::Host {
            return Err(PunktfunkError::InvalidArg(
                "seal_frame called on a client session",
            ));
        }
        // Packetize straight into the pooled wire buffers (reused across frames via
        // `reclaim_wires`) and seal each in place: the plaintext `header ++ shard` is written
        // once, at its final wire offset — no intermediate per-packet Vec at all. Byte-identical
        // to the wrapper (`packetize` + seal) path: same plaintext, same emission order, and the
        // nonce counter advances per emitted packet exactly as before (pinned by the
        // wire-equivalence tests below). Destructure into disjoint field borrows first — the
        // emit closure needs `crypto`/`next_seq`/the pool while `packetizer` is `&mut`.
        let perf_armed = self.seal_perf.is_some();
        let fec_ns = std::sync::atomic::AtomicU64::new(0);
        let mut seal_ns = 0u64;
        let two_lane = self.seal_two_lane;
        let Session {
            packetizer,
            coder,
            crypto,
            next_seq,
            wire_pool,
            seal_lane,
            lane_scratch,
            ..
        } = self;
        // Stage timing (SealPerf): the coder shim times FEC, the seal phase times itself.
        let timed_coder;
        let coder_ref: &dyn ErasureCoder = if perf_armed {
            timed_coder = TimedCoder {
                inner: coder.as_ref(),
                ns: &fec_ns,
            };
            &timed_coder
        } else {
            coder.as_ref()
        };
        let mut wires = std::mem::take(wire_pool);
        let mut used = 0usize;
        // Phase 1 — packetize: write each packet's plaintext at its final wire offset
        // (`seq(8) ‖ header(40) ‖ shard ‖ tag scratch(16)` with crypto on; `header ‖ shard`
        // off). The nonce counter advances per packet in emission order exactly as before;
        // sealing itself is a separate pass so it can split across lanes.
        let seq_base = *next_seq;
        let encrypting = crypto.is_some();
        let result = packetizer.packetize_each(data, pts_ns, user_flags, frame_index, coder_ref, {
            let wires = &mut wires;
            let used = &mut used;
            move |hdr, body| {
                if *used == wires.len() {
                    wires.push(Vec::new());
                }
                let wire = &mut wires[*used];
                *used += 1;
                let seq = *next_seq;
                *next_seq = next_seq.wrapping_add(1);
                wire.clear();
                if encrypting {
                    wire.extend_from_slice(&seq.to_be_bytes());
                    wire.extend_from_slice(hdr.as_bytes());
                    wire.extend_from_slice(body);
                    wire.resize(wire.len() + crate::crypto::TAG_LEN, 0);
                } else {
                    wire.extend_from_slice(hdr.as_bytes());
                    wire.extend_from_slice(body);
                }
                Ok(())
            }
        });
        result?;
        // A smaller frame uses fewer buffers than the pool holds: drop the unused tail, same
        // as the previous `resize_with(packets.len(), ..)` did. (Before the seal phase, so a
        // two-lane split hands the worker exactly the frame's back half.)
        wires.truncate(used);
        // Phase 2 — seal. Large frames split across two lanes (plan Phase 1.5): the worker
        // seals the back half under nonces `seq_base + i` while this thread seals the front —
        // byte-identical output to the sequential pass (pinned by the wire-equivalence test).
        if let Some(c) = crypto {
            if two_lane && used >= TWO_LANE_MIN_PACKETS && seal_lane.is_none() {
                *seal_lane = SealLane::spawn(c.clone()); // stays None if spawn fails → single-lane
            }
            let mut split_done = false;
            if two_lane && used >= TWO_LANE_MIN_PACKETS {
                if let Some(lane) = seal_lane.as_ref() {
                    let half = used / 2;
                    let mut tail = std::mem::take(lane_scratch);
                    tail.extend(wires.drain(half..));
                    let job = SealJob {
                        bufs: tail,
                        seq_base: seq_base.wrapping_add(half as u64),
                        timed: perf_armed,
                        ns: 0,
                        result: Ok(()),
                    };
                    if lane.to_worker.send(job).is_ok() {
                        // Seal the front half while the worker runs; collect BOTH results
                        // before erroring so the lane is always drained and reusable.
                        let t0 = perf_armed.then(std::time::Instant::now);
                        let front = seal_wire_slice(c, &mut wires, seq_base);
                        if let Some(t0) = t0 {
                            seal_ns += t0.elapsed().as_nanos() as u64;
                        }
                        let mut done = lane
                            .from_worker
                            .recv()
                            .map_err(|_| PunktfunkError::Unsupported("seal lane died"))?;
                        seal_ns += done.ns;
                        wires.append(&mut done.bufs);
                        *lane_scratch = done.bufs;
                        front?;
                        done.result?;
                        split_done = true;
                    }
                    // A failed send means the worker is gone — fall through to single-lane.
                }
            }
            if !split_done {
                let t0 = perf_armed.then(std::time::Instant::now);
                seal_wire_slice(c, &mut wires, seq_base)?;
                if let Some(t0) = t0 {
                    seal_ns += t0.elapsed().as_nanos() as u64;
                }
            }
        }
        if let Some(p) = self.seal_perf.as_mut() {
            p.fec_ns += fec_ns.load(std::sync::atomic::Ordering::Relaxed);
            p.seal_ns += seal_ns;
            p.frames += 1;
            p.packets += used as u64;
        }
        StatsCounters::add(&self.stats.frames_submitted, 1);
        let bytes: u64 = wires.iter().map(|w| w.len() as u64).sum();
        StatsCounters::add(&self.stats.packets_sent, wires.len() as u64);
        StatsCounters::add(&self.stats.bytes_sent, bytes);
        Ok(wires)
    }

    /// Return the wire buffers from [`seal_frame`](Self::seal_frame) to the reuse pool once the caller
    /// has finished sending them, so the next frame reseals in place with no allocation. Optional —
    /// dropping the buffers instead just forfeits the reuse (correctness is unaffected).
    pub fn reclaim_wires(&mut self, wires: Vec<Vec<u8>>) {
        self.wire_pool = wires;
    }

    /// Host: transmit one chunk of already-[`seal_frame`](Self::seal_frame)ed packets in a single
    /// batched `sendmmsg`, returning how many the kernel accepted. The rest (`packets.len() - n`)
    /// are counted as send-buffer drops. Call once for the whole frame, or per paced chunk.
    pub fn send_sealed(&self, packets: &[&[u8]]) -> Result<usize> {
        // GSO when enabled (UdpTransport/Linux), else sendmmsg — same short-count drop contract.
        let sent = self.transport.send_gso(packets)?;
        if sent < packets.len() {
            StatsCounters::add(
                &self.stats.packets_send_dropped,
                (packets.len() - sent) as u64,
            );
        }
        Ok(sent)
    }

    /// Host: FEC-protect, packetize, seal, and send one encoded access unit (the whole frame in one
    /// batched send). Convenience composition of [`seal_frame`](Self::seal_frame) +
    /// [`send_sealed`](Self::send_sealed) for callers that don't pace (synthetic source, probe).
    pub fn submit_frame(&mut self, data: &[u8], pts_ns: u64, user_flags: u32) -> Result<()> {
        let wires = self.seal_frame(data, pts_ns, user_flags)?;
        let refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
        let t0 = self.seal_perf.is_some().then(std::time::Instant::now);
        let r = self.send_sealed(&refs);
        drop(refs); // release the borrow of `wires` before returning the buffers to the pool
        if let Some(t0) = t0 {
            self.note_sock_ns(t0.elapsed().as_nanos() as u64);
        }
        self.reclaim_wires(wires);
        r.map(|_| ())
    }

    /// Host: seal + send one **speed-test probe filler** access unit in the probe index space
    /// (its own frame counter + the [`crate::packet::FLAG_PROBE`] user-flag) so a burst never
    /// consumes video `frame_index`es — the client reassembles probe frames in a separate window
    /// and its gap detectors never see them. Only call this against a client that advertised
    /// [`crate::quic::VIDEO_CAP_PROBE_SEQ`]; an older client's single-window reassembler would
    /// drop probe-space indexes as stale against the video stream.
    pub fn submit_probe_frame(&mut self, data: &[u8], pts_ns: u64) -> Result<()> {
        let idx = self.packetizer.alloc_probe_index();
        let wires =
            self.seal_frame_inner(data, pts_ns, crate::packet::FLAG_PROBE as u32, Some(idx))?;
        let refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
        let t0 = self.seal_perf.is_some().then(std::time::Instant::now);
        let r = self.send_sealed(&refs);
        drop(refs);
        if let Some(t0) = t0 {
            self.note_sock_ns(t0.elapsed().as_nanos() as u64);
        }
        self.reclaim_wires(wires);
        r.map(|_| ())
    }

    /// Host: live-adjust the FEC recovery percentage (adaptive FEC). Affects the next
    /// [`submit_frame`](Self::submit_frame)/[`seal_frame`](Self::seal_frame); the receiver needs no
    /// notification (each packet's header carries its block's data/recovery shard counts).
    pub fn set_fec_percent(&mut self, pct: u8) {
        self.packetizer.set_fec_percent(pct);
    }

    /// The current FEC recovery percentage (host side).
    pub fn fec_percent(&self) -> u8 {
        self.packetizer.fec_percent()
    }

    /// Host: drain one pending input event from the client, if any.
    pub fn poll_input(&mut self) -> Result<Option<InputEvent>> {
        if self.config.role != Role::Host {
            return Err(PunktfunkError::InvalidArg(
                "poll_input called on a client session",
            ));
        }
        while let Some(wire) = self.transport.recv()? {
            let pkt = match self.open_from_wire(&wire) {
                Ok(p) => p,
                Err(_) => continue, // drop undecryptable noise
            };
            // Anti-replay: a captured input datagram replayed by an on-path attacker opens cleanly
            // (its sequence + tag are still valid) — the window is what rejects the second copy.
            // `len >= 8` is guaranteed because the sealed-path open above succeeded.
            if self.replay.is_some() && !self.accept_seq(seq_of(&wire)) {
                StatsCounters::add(&self.stats.packets_dropped, 1);
                continue;
            }
            StatsCounters::add(&self.stats.packets_received, 1);
            if let Some(ev) = InputEvent::decode(&pkt) {
                return Ok(Some(ev));
            }
            // Not an input datagram (e.g. stray video) — ignore and keep draining.
        }
        Ok(None)
    }

    // -- Client path ------------------------------------------------------

    /// Client: drain the transport until a whole access unit is recovered, or no more
    /// packets are pending ([`PunktfunkError::NoFrame`]).
    pub fn poll_frame(&mut self) -> Result<Frame> {
        if self.config.role != Role::Client {
            return Err(PunktfunkError::InvalidArg(
                "poll_frame called on a host session",
            ));
        }
        // Lazily allocate the recv ring on first client poll (host sessions never get here).
        if self.recv_scratch.is_empty() {
            // Each buffer holds a max datagram + 1 (an oversized read fills it → reassembler rejects).
            self.recv_scratch = (0..RECV_BATCH)
                .map(|_| vec![0u8; MAX_DATAGRAM_BYTES + 1])
                .collect();
            self.recv_lens = vec![0usize; RECV_BATCH];
        }
        loop {
            // Refill the ring with one `recvmmsg` batch when the current one is drained.
            if self.recv_idx >= self.recv_count {
                let t0 = self.perf.is_some().then(std::time::Instant::now);
                self.recv_count = self
                    .transport
                    .recv_batch(&mut self.recv_scratch, &mut self.recv_lens)?;
                if let (Some(p), Some(t0)) = (self.perf.as_mut(), t0) {
                    p.recv_ns += t0.elapsed().as_nanos() as u64;
                    p.batches += 1;
                }
                self.recv_idx = 0;
                if self.recv_count == 0 {
                    return Err(PunktfunkError::NoFrame);
                }
            }
            let i = self.recv_idx;
            self.recv_idx += 1;
            let len = self.recv_lens[i];
            // An oversized datagram fills the whole buffer (recvmmsg truncates + caps msg_len at the
            // buffer size) — drop it rather than hand up a truncated, corrupt packet, mirroring the
            // scalar `recv`'s `n >= RECV_BUF` check.
            if len > MAX_DATAGRAM_BYTES {
                continue;
            }
            // Open in place inside the ring buffer — no per-datagram allocation at line rate
            // (~125k pkt/s at 1 Gbps; the recv ring killed the recv alloc, this kills the decrypt
            // one). The plaintext lands at [8..8+n] of the sealed wire (behind the seq prefix); an
            // unencrypted (probe) datagram IS the packet. Field-precise borrows keep the slice into
            // `recv_scratch` alive across the replay/reassembler calls below.
            // Perf note: the two `continue`s below (short / undecryptable noise) skip the decrypt
            // accounting — they are the exception path, not line-rate traffic.
            let t_dec = self.perf.is_some().then(std::time::Instant::now);
            let (pkt_range, seq) = match &self.crypto {
                Some(c) => {
                    // A sealed datagram is at least seq prefix + tag; anything shorter is noise.
                    if len < 8 + crate::crypto::TAG_LEN {
                        continue;
                    }
                    let seq = u64::from_be_bytes(self.recv_scratch[i][..8].try_into().unwrap());
                    match c.open_in_place(seq, &mut self.recv_scratch[i][8..len]) {
                        Ok(n) => (8..8 + n, Some(seq)),
                        Err(_) => continue, // undecryptable noise — drop, keep draining
                    }
                }
                None => (0..len, None),
            };
            if let (Some(p), Some(t)) = (self.perf.as_mut(), t_dec) {
                p.decrypt_ns += t.elapsed().as_nanos() as u64;
            }
            // Anti-replay (same rationale as poll_input): reject a datagram whose authenticated
            // sequence was already seen. Video also dedups per-frame downstream, but filtering here
            // is uniform and cheap.
            if let (Some(w), Some(seq)) = (self.replay.as_mut(), seq) {
                if !w.accept(seq) {
                    StatsCounters::add(&self.stats.packets_dropped, 1);
                    continue;
                }
            }
            let pkt = &self.recv_scratch[i][pkt_range];
            StatsCounters::add(&self.stats.packets_received, 1);
            StatsCounters::add(&self.stats.bytes_received, pkt.len() as u64);
            // The reassembler validates the packet via its parsed header (`magic`),
            // ignoring anything that isn't a well-formed video packet.
            let t_push = self.perf.is_some().then(std::time::Instant::now);
            let pushed = self
                .reassembler
                .push(pkt, self.coder.as_ref(), &self.stats)?;
            if let (Some(p), Some(t)) = (self.perf.as_mut(), t_push) {
                p.reasm_ns += t.elapsed().as_nanos() as u64;
                // Counts datagrams that reached the reassembler (replay-rejected ones don't).
                p.packets += 1;
            }
            if let Some(frame) = pushed {
                StatsCounters::add(&self.stats.frames_completed, 1);
                return Ok(frame);
            }
        }
    }

    /// Client: discard the ENTIRE pending receive backlog — the current recv ring plus everything
    /// queued in the kernel socket buffer — and reset the reassembler. Returns how many datagrams
    /// were thrown away (counted into `packets_dropped`).
    ///
    /// This is the latency-bound escape hatch: the receive path has no other way to skip ahead.
    /// Packets arrive strictly in order, so once a standing queue forms (the pump transiently
    /// slower than the wire, a Wi-Fi stall, power-save delivery clumping), the client plays that
    /// far behind FOREVER — it consumes at exactly the arrival rate, so the backlog never shrinks
    /// (observed live: a stream stuck 6–7 s behind, socket buffers full end to end). Discarding
    /// is memcpy-speed (no decrypt/reassembly/allocation), so this empties even a 32 MB buffer in
    /// milliseconds; the caller then requests a keyframe and the stream resumes live. The iteration
    /// cap (1024 batches ≈ 131k datagrams ≈ 190 MB at the 128-deep ring) only guards against a
    /// line-rate sender outpacing the discard loop indefinitely.
    pub fn flush_backlog(&mut self) -> Result<u64> {
        if self.config.role != Role::Client {
            return Err(PunktfunkError::InvalidArg(
                "flush_backlog called on a host session",
            ));
        }
        // The undelivered tail of the current ring is backlog too.
        let mut flushed = self.recv_count.saturating_sub(self.recv_idx) as u64;
        self.recv_count = 0;
        self.recv_idx = 0;
        if !self.recv_scratch.is_empty() {
            for _ in 0..1024 {
                let n = self
                    .transport
                    .recv_batch(&mut self.recv_scratch, &mut self.recv_lens)?;
                if n == 0 {
                    break;
                }
                flushed += n as u64;
            }
        }
        self.reassembler.reset();
        StatsCounters::add(&self.stats.packets_dropped, flushed);
        Ok(flushed)
    }

    /// Client: serialize and send one input event to the host.
    pub fn send_input(&mut self, event: &InputEvent) -> Result<()> {
        if self.config.role != Role::Client {
            return Err(PunktfunkError::InvalidArg(
                "send_input called on a host session",
            ));
        }
        let pkt = event.encode();
        let mut wire = Vec::new(); // input is rare + per-event; no pool needed
        self.seal_into(&pkt, &mut wire)?;
        StatsCounters::add(&self.stats.packets_sent, 1);
        StatsCounters::add(&self.stats.bytes_sent, wire.len() as u64);
        if !self.transport.send(&wire)? {
            StatsCounters::add(&self.stats.packets_send_dropped, 1);
        }
        Ok(())
    }
}

/// Extract the AEAD-authenticated 8-byte big-endian sequence prefix from a sealed wire datagram.
/// Only called on the encrypted receive path, where a preceding successful open has already
/// established `wire.len() >= 8`.
fn seq_of(wire: &[u8]) -> u64 {
    u64::from_be_bytes(wire[..8].try_into().unwrap())
}

/// Depth of the anti-replay window, in sequences. The sender advances its sequence once per
/// datagram, so this must cover the reassembler's 120 ms loss window
/// ([`LOSS_WINDOW_NS`](crate::packet)) at line-rate packet rates — otherwise the replay filter
/// silently re-tightens the "late ≠ lost" fix: a Wi-Fi-retry-delayed shard the reassembler would
/// still use gets dropped here as "older than the window" first (4096 was only ~33 ms at the
/// ~125k pkt/s of a 1 Gbps stream; 32768 topped out around ~2 Gbps — which the client now
/// exceeds: the 2026-07-14 zero-copy + hardware-AES work measured ~4.8 Gbps wire ≈ 430k pkt/s
/// delivered). 131072 covers 120 ms up to ~1.09M pkt/s (≈12 Gbps wire) and is effectively
/// unbounded for the sparse input stream, while still bounding how far back a replay could
/// hide; the bitmap costs 16 KiB per session.
const REPLAY_WINDOW: u64 = 131072;
const REPLAY_WORDS: usize = (REPLAY_WINDOW / 64) as usize;

/// Sliding-window anti-replay filter over the AEAD-authenticated wire sequence. The sender counts
/// its datagrams from 0, and the protocol never legitimately re-sends a sequence (FEC recovery
/// shards get fresh ones), so a sequence seen twice is a replay. The AEAD tag already authenticates
/// the sequence — a forged one can't open — so this only has to reject *duplicates* of validly
/// sealed datagrams (and anything older than the window, which we can no longer prove is fresh).
/// Genuine reordering within the window is accepted. Bitmap-per-sequence, indexed `seq % WINDOW`.
struct ReplayWindow {
    /// Highest sequence accepted so far; `seen` stays false until the first datagram.
    highest: u64,
    seen: bool,
    /// One bit per in-window sequence in `(highest - WINDOW, highest]`.
    bits: [u64; REPLAY_WORDS],
}

impl ReplayWindow {
    fn new() -> ReplayWindow {
        ReplayWindow {
            highest: 0,
            seen: false,
            bits: [0; REPLAY_WORDS],
        }
    }

    #[inline]
    fn word_bit(seq: u64) -> (usize, u64) {
        let idx = (seq % REPLAY_WINDOW) as usize;
        (idx / 64, 1u64 << (idx % 64))
    }
    fn is_set(&self, seq: u64) -> bool {
        let (w, b) = Self::word_bit(seq);
        self.bits[w] & b != 0
    }
    fn set(&mut self, seq: u64) {
        let (w, b) = Self::word_bit(seq);
        self.bits[w] |= b;
    }
    fn unset(&mut self, seq: u64) {
        let (w, b) = Self::word_bit(seq);
        self.bits[w] &= !b;
    }

    /// Record `seq`, returning `true` if it's fresh (accept) or `false` if it's a replay / too old.
    fn accept(&mut self, seq: u64) -> bool {
        if !self.seen {
            self.seen = true;
            self.highest = seq;
            self.set(seq);
            return true;
        }
        if seq > self.highest {
            // Advance the window. Sequences between the old and new high slide in unseen, so clear
            // their (possibly stale, from a full window ago) slots — unless we jumped an entire
            // window, in which case wipe the bitmap wholesale.
            if seq - self.highest >= REPLAY_WINDOW {
                self.bits = [0; REPLAY_WORDS];
            } else {
                let mut s = self.highest + 1;
                while s < seq {
                    self.unset(s);
                    s += 1;
                }
            }
            self.highest = seq;
            self.set(seq);
            true
        } else if self.highest - seq >= REPLAY_WINDOW || self.is_set(seq) {
            // Older than the window (can't prove it isn't a replay) or already seen (a duplicate) —
            // either way, drop it.
            false
        } else {
            self.set(seq); // in-window and not yet seen — a genuine reorder
            true
        }
    }
}

#[cfg(test)]
mod wire_equivalence_tests {
    use super::*;
    use crate::config::{FecConfig, FecScheme, ProtocolPhase};
    use crate::transport::loopback_pair;

    fn host_cfg(scheme: FecScheme, fec_percent: u8, encrypt: bool) -> Config {
        Config {
            role: Role::Host,
            phase: match scheme {
                FecScheme::Gf8 => ProtocolPhase::P1GameStream,
                FecScheme::Gf16 => ProtocolPhase::P2Punktfunk,
            },
            fec: FecConfig {
                scheme,
                fec_percent,
                max_data_per_block: 8,
            },
            shard_payload: 64,
            max_frame_bytes: 8 * 1024 * 1024,
            encrypt,
            key: [7u8; 16],
            salt: [3, 1, 4, 1],
            loopback_drop_period: 0,
        }
    }

    fn host_session(cfg: Config) -> Session {
        let (h, _c) = loopback_pair(0, 0);
        Session::new(cfg, Box::new(h)).unwrap()
    }

    /// The reference wire path: build owned packets via the `packetize` wrapper, then seal
    /// each into its own buffer — the pre-zero-copy implementation of `seal_frame`, spelled
    /// out with the session's own private pieces so the two paths share nothing but state.
    fn seal_via_wrapper(sess: &mut Session, frame: &[u8], pts_ns: u64, flags: u32) -> Vec<Vec<u8>> {
        let packets = sess
            .packetizer
            .packetize(frame, pts_ns, flags, sess.coder.as_ref())
            .unwrap();
        let mut wires = Vec::new();
        for pkt in &packets {
            let mut wire = Vec::new();
            sess.seal_into(pkt, &mut wire).unwrap();
            wires.push(wire);
        }
        wires
    }

    /// `seal_frame`'s packetize-straight-into-the-wire-pool path must produce byte-identical
    /// sealed output to the wrapper path (same plaintext = header ++ shard, same nonce
    /// sequence) — for multi-block frames, partial tail shards, exact-multiple frames, the
    /// empty frame, fec 0%/50%, both schemes, crypto on and off (plan §1.4).
    #[test]
    fn zero_copy_seal_matches_wrapper_path() {
        for scheme in [FecScheme::Gf8, FecScheme::Gf16] {
            for fec_percent in [0u8, 50] {
                for encrypt in [true, false] {
                    let mut opt = host_session(host_cfg(scheme, fec_percent, encrypt));
                    let mut refr = host_session(host_cfg(scheme, fec_percent, encrypt));

                    // shard_payload 64 × max_data_per_block 8: >512 bytes spans FEC blocks.
                    let frames: Vec<Vec<u8>> = vec![
                        pattern(3000),  // multi-block + partial tail shard
                        pattern(1024),  // exact multiple (2 full blocks)
                        pattern(100),   // single block, partial tail
                        Vec::new(),     // empty frame → 1 zeroed shard
                        pattern(64),    // exactly one full shard
                        pattern(20000), // > TWO_LANE_MIN_PACKETS wire packets → two-lane seal
                    ];
                    for (i, frame) in frames.iter().enumerate() {
                        let got = opt.seal_frame(frame, 1000 * i as u64, i as u32).unwrap();
                        let want = seal_via_wrapper(&mut refr, frame, 1000 * i as u64, i as u32);
                        assert_eq!(
                            got, want,
                            "wire mismatch: scheme={scheme:?} fec={fec_percent}% encrypt={encrypt} frame#{i}"
                        );
                        // Return the buffers so later frames exercise the pooled-reuse path
                        // (including a bigger frame after a smaller one and vice versa).
                        opt.reclaim_wires(got);
                    }
                    // The 20000-byte frame (~469 wire packets at shard 64) crosses
                    // TWO_LANE_MIN_PACKETS: the equality above must have held THROUGH the
                    // two-lane split, not via a silent single-lane fallback.
                    if encrypt {
                        assert!(
                            opt.seal_lane.is_some(),
                            "two-lane seal lane should have spawned for the large frame"
                        );
                    }
                }
            }
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 + 7) as u8).collect()
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;

    #[test]
    fn accepts_in_order_and_rejects_duplicates() {
        let mut w = ReplayWindow::new();
        for seq in 0..1000 {
            assert!(w.accept(seq), "fresh in-order seq {seq} must be accepted");
        }
        // Every one of those is now a replay.
        for seq in 0..1000 {
            assert!(!w.accept(seq), "replayed seq {seq} must be rejected");
        }
    }

    #[test]
    fn accepts_reorder_within_window_once() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(100));
        // Earlier-but-in-window sequences (a genuine reorder) are accepted exactly once.
        assert!(w.accept(80));
        assert!(!w.accept(80), "second copy of a reordered seq is a replay");
        assert!(w.accept(99));
        assert!(
            !w.accept(100),
            "the high-water seq itself can't be replayed"
        );
    }

    #[test]
    fn rejects_older_than_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(REPLAY_WINDOW * 2));
        // Anything a full window or more behind the high-water mark is dropped (can't prove fresh).
        assert!(!w.accept(REPLAY_WINDOW * 2 - REPLAY_WINDOW));
        assert!(!w.accept(0));
        // But just inside the window is still accepted.
        assert!(w.accept(REPLAY_WINDOW * 2 - (REPLAY_WINDOW - 1)));
    }

    #[test]
    fn large_forward_jump_wipes_stale_bits() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(5));
        // Jump far forward (more than a window). The slot for an old seq that aliases 5 mod WINDOW
        // must read as unseen afterward, i.e. the jump cleared it — so a NEW seq there is accepted.
        let far = 10 * REPLAY_WINDOW + 5;
        assert!(w.accept(far));
        assert!(
            !w.accept(5),
            "the pre-jump seq is now far older than the window"
        );
        // A fresh seq aliasing 5 (mod WINDOW) but inside the new window is accepted, proving the
        // stale bit was cleared rather than mistaken for a replay.
        assert!(w.accept(far - REPLAY_WINDOW + 1));
    }

    #[test]
    fn first_seq_need_not_be_zero() {
        // Startup loss can mean the first datagram we ever open isn't seq 0.
        let mut w = ReplayWindow::new();
        assert!(w.accept(42));
        assert!(!w.accept(42));
        assert!(w.accept(43));
    }

    #[test]
    fn seq_of_reads_the_big_endian_prefix() {
        let mut wire = 0x0102_0304_0506_0708u64.to_be_bytes().to_vec();
        wire.extend_from_slice(b"ciphertext-and-tag");
        assert_eq!(seq_of(&wire), 0x0102_0304_0506_0708);
    }
}
