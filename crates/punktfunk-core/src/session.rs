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
use crate::packet::{
    PacketHeader, Packetizer, Reassembler, ReassemblerLimits, StreamedAu, MAX_DATAGRAM_BYTES,
};
use crate::stats::{Stats, StatsCounters};
use crate::transport::Transport;
use zerocopy::IntoBytes;

/// Slice-progressive delivery metadata ([`Session::set_deliver_frame_parts`]): this [`Frame`]
/// carries one contiguous piece of an access unit, handed up while the rest is still on the
/// wire so a `PARTIAL_FRAME`-capable decoder can start ahead of the last packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramePart {
    /// Byte offset of `data` within the whole AU. The reassembler emits parts in order with
    /// no gaps — but the pre-decode hand-off may drop entries under memory pressure or a
    /// jump-to-live clear, so a consumer must still verify `offset` equals its open AU's next
    /// expected byte and treat a mismatch as the AU lost (abandon + flush the decoder's
    /// partial input, then discard until the next `first`). The same discard applies to a
    /// non-`first` part arriving with no AU open.
    pub offset: u32,
    /// The AU's first part. A `first` part for a NEW `frame_index` while an earlier AU is
    /// still open means that AU died mid-flight (aged out or was cleared) — abandon it and
    /// flush the decoder's partial input; no explicit abort part is ever sent.
    pub first: bool,
    /// The AU's final part — the whole AU has now been delivered ([`Frame::complete`] is set
    /// on exactly this part) and the input may be submitted to the decoder as finished.
    pub last: bool,
}

/// A reassembled, FEC-recovered access unit, ready to hand to the platform decoder.
pub struct Frame {
    pub data: Vec<u8>,
    pub frame_index: u32,
    pub pts_ns: u64,
    pub flags: u32,
    /// `false` = a partial delivery: the frame aged out of the loss window with shards
    /// missing, and the session opted into receiving it anyway
    /// ([`Session::set_deliver_partial_frames`]). Only chunk-aligned AUs
    /// ([`crate::packet::USER_FLAG_CHUNK_ALIGNED`]) are ever delivered partial; missing
    /// shard ranges are zero-filled at their exact offsets.
    pub complete: bool,
    /// `Some` = slice-progressive delivery is on and this is ONE PIECE of an AU (see
    /// [`FramePart`]); `data` holds only the part's bytes. `None` = a whole AU (or an
    /// aged-out chunk-aligned partial — `complete` distinguishes).
    pub part: Option<FramePart>,
    /// Wall-clock instant (ns since the Unix epoch, CLOCK_REALTIME basis — the same clock the
    /// skew handshake compares and the host stamps `pts_ns` with) at which this AU finished
    /// reassembly, stamped by [`Session::poll_frame`] as the frame leaves the session. Embedders
    /// that previously stamped receipt themselves at the hand-off pull should use this instead:
    /// the pull stamp additionally contains the pre-decode queue wait, silently folding any
    /// client-side standing backlog into the apparent NETWORK latency. The reassembler itself
    /// leaves this 0 (it owns no clock — the stamp is the session boundary's job).
    pub received_ns: u64,
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

/// Stamp [`Frame::received_ns`] as the frame crosses the session boundary in
/// [`Session::poll_frame`] — completed frames return the moment their last shard lands, so
/// stamping at return IS stamping at reassembly completion (µs apart). CLOCK_REALTIME to match
/// `pts_ns` / the skew handshake (deliberately not monotonic — cross-machine latency math).
fn stamp_received(mut f: Frame) -> Frame {
    f.received_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    f
}

mod perf;
mod replay;
mod seal;

pub use perf::{PumpPerf, SealPerf};

use perf::TimedCoder;
use replay::{seq_of, ReplayWindow};
use seal::{seal_wire_slice, SealJob, SealLane, TWO_LANE_MIN_PACKETS};

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

    /// Re-arm the probe-scoped arrival stamps (see [`Stats::probe_first_arrival_ns`]): zero
    /// them so the NEXT burst's first packet claims the first-arrival slot. Called by the
    /// client pump when it arms a probe, strictly before the burst can have reached the host
    /// (the `ProbeRequest` is still queued locally) — so the reset cannot race a probe packet.
    /// The cumulative probe byte/packet counters are left alone: per-burst deltas come from
    /// base snapshots, the same pattern the total counters use.
    pub fn reset_probe_arrivals(&self) {
        let l = std::sync::atomic::Ordering::Relaxed;
        self.stats.probe_first_arrival_ns.store(0, l);
        self.stats.probe_last_arrival_ns.store(0, l);
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
        self.seal_run(true, |p, coder, emit| {
            p.packetize_each(data, pts_ns, user_flags, frame_index, coder, emit)
        })
    }

    /// Host: open a **streamed** access unit ([`crate::quic::VIDEO_CAP_STREAMED_AU`] — only
    /// toward a client that advertised it; anyone else must get the whole-AU
    /// [`seal_frame_at`](Self::seal_frame_at) path). The AU's bytes are fed in with
    /// [`seal_streamed_chunk`](Self::seal_streamed_chunk) as the encoder produces them and
    /// closed with [`seal_streamed_finish`](Self::seal_streamed_finish); the three calls'
    /// returned wire batches are ONE frame, and the nonce order is emission order across the
    /// calls — send each batch as it is returned, before sealing the next.
    pub fn begin_streamed_frame_at(
        &mut self,
        pts_ns: u64,
        user_flags: u32,
        frame_index: u32,
    ) -> Result<StreamedAu> {
        if self.config.role != Role::Host {
            return Err(PunktfunkError::InvalidArg(
                "seal_frame called on a client session",
            ));
        }
        Ok(self
            .packetizer
            .begin_streamed(pts_ns, user_flags, Some(frame_index)))
    }

    /// Feed one encoder chunk into a streamed AU, sealing slice-granularity blocks under
    /// sentinel headers (see [`begin_streamed_frame_at`](Self::begin_streamed_frame_at)).
    /// `slice_end` marks an encoder slice boundary — the flush points of the P2 slice pipeline
    /// (with it false the legacy full-FEC-block granularity applies). The returned batch is
    /// often EMPTY (the chunk is buffered until enough accumulates) — that's normal.
    pub fn seal_streamed_chunk(
        &mut self,
        au: &mut StreamedAu,
        chunk: &[u8],
        slice_end: bool,
    ) -> Result<Vec<Vec<u8>>> {
        self.seal_run(false, |p, coder, emit| {
            p.push_streamed(au, chunk, slice_end, coder, emit)
        })
    }

    /// Close a streamed AU: seal the final block with the real totals (+ `FLAG_EOF`), which
    /// retro-validate the frame at the receiver. Counts the frame as submitted.
    pub fn seal_streamed_finish(&mut self, au: StreamedAu) -> Result<Vec<Vec<u8>>> {
        self.seal_run(true, |p, coder, emit| p.finish_streamed(au, coder, emit))
    }

    /// The shared packetize → pooled-wire → seal machinery behind [`seal_frame`](Self::seal_frame)
    /// and the streamed sealers: `run` drives the packetizer against an emit sink that writes
    /// each packet's plaintext at its final wire offset; the seal pass (two-lane for large runs)
    /// then encrypts in place. `count_frame` gates the per-frame stats — a streamed AU counts
    /// once, at its finish call.
    fn seal_run(
        &mut self,
        count_frame: bool,
        run: impl FnOnce(
            &mut Packetizer,
            &dyn ErasureCoder,
            &mut dyn FnMut(&PacketHeader, &[u8]) -> Result<()>,
        ) -> Result<()>,
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
        let result = {
            let wires = &mut wires;
            let used = &mut used;
            let mut emit = move |hdr: &PacketHeader, body: &[u8]| -> Result<()> {
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
            };
            run(packetizer, coder_ref, &mut emit)
        };
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
                // Take the lane for the frame: a healthy round-trip puts it back; either
                // failure arm drops the corpse so the next large frame respawns a fresh one
                // instead of retrying a dead channel forever.
                if let Some(lane) = seal_lane.take() {
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
                    match lane.to_worker.send(job) {
                        Ok(()) => {
                            // Seal the front half while the worker runs; collect BOTH results
                            // before erroring so the lane is always drained and reusable.
                            let t0 = perf_armed.then(std::time::Instant::now);
                            let front = seal_wire_slice(c, &mut wires, seq_base);
                            if let Some(t0) = t0 {
                                seal_ns += t0.elapsed().as_nanos() as u64;
                            }
                            match lane.from_worker.recv() {
                                Ok(mut done) => {
                                    *seal_lane = Some(lane);
                                    seal_ns += done.ns;
                                    wires.append(&mut done.bufs);
                                    *lane_scratch = done.bufs;
                                    front?;
                                    done.result?;
                                    split_done = true;
                                }
                                Err(_) => {
                                    // The worker died holding the back half — the frame is
                                    // unrecoverable (its packets are gone), but the error now
                                    // SURFACES instead of `Ok` with half an access unit.
                                    front?;
                                    return Err(PunktfunkError::Unsupported("seal lane died"));
                                }
                            }
                        }
                        Err(std::sync::mpsc::SendError(job)) => {
                            // The worker is gone but the channel hands the job back: reclaim
                            // the back half so the single-lane pass below seals the WHOLE
                            // frame — previously this fall-through sealed and returned only
                            // the front half, silently, as `Ok`.
                            wires.extend(job.bufs);
                        }
                    }
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
            p.frames += count_frame as u64;
            p.packets += used as u64;
        }
        if count_frame {
            StatsCounters::add(&self.stats.frames_submitted, 1);
        }
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
    /// Client opt-in: deliver aged-out incomplete chunk-aligned frames as
    /// [`Frame`]`{ complete: false }` instead of only dropping them (the PyroWave
    /// datagram-aligned mode, plan §4.4 — a lost datagram costs a few blocks of blur,
    /// not the frame). No effect on other codecs' AUs (they never carry the flag).
    pub fn set_deliver_partial_frames(&mut self, on: bool) {
        self.reassembler.set_deliver_partial(on);
    }

    /// Client opt-in: deliver each AU's newly-contiguous prefix as [`Frame`]s with
    /// [`Frame::part`]` = Some` while the rest is still on the wire, instead of one whole-AU
    /// delivery (the slice-progressive decode path — [`crate::packet::USER_FLAG_SLICE_STREAM`]).
    /// With it on, EVERY video frame delivery carries `part: Some` (a frame with no early
    /// parts arrives as the degenerate `{offset: 0, first, last}` whole). Do not combine with
    /// an all-intra (PyroWave) stream: its newest-wins draining assumes whole AUs.
    pub fn set_deliver_frame_parts(&mut self, on: bool) {
        self.reassembler.set_deliver_parts(on);
    }

    /// The session's negotiated wire shard payload size (bytes of AU per datagram) —
    /// the window size for chunk-aligned AUs (`USER_FLAG_CHUNK_ALIGNED`).
    pub fn shard_payload(&self) -> usize {
        self.config.shard_payload
    }

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
                    // Nothing new on the wire — hand over an aged-out partial if one is
                    // waiting (it can only get staler).
                    if let Some(p) = self.reassembler.take_partial() {
                        return Ok(stamp_received(p));
                    }
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
                // A prefix part is not a completed frame — only the delivery that closes the
                // AU counts, or parts would multiply the completion rate.
                if frame.complete {
                    StatsCounters::add(&self.stats.frames_completed, 1);
                }
                return Ok(stamp_received(frame));
            }
            // A push that completed nothing may still have aged a partial out — deliver it
            // ahead of further draining (its successors are already arriving).
            if let Some(p) = self.reassembler.take_partial() {
                return Ok(stamp_received(p));
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

#[cfg(test)]
mod wire_equivalence_tests {
    use super::*;
    use crate::config::{FecConfig, FecScheme, ProtocolPhase};
    use crate::crypto::SessionKey;
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
            key: SessionKey::Aes128Gcm([7u8; 16]),
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

    /// A dead seal lane (worker gone, channels dangling) must degrade to a single-lane seal of
    /// the WHOLE frame — the old fall-through sealed and returned only the front half as `Ok` —
    /// and the corpse must be dropped so the next large frame respawns a fresh lane.
    #[test]
    fn dead_seal_lane_falls_back_to_single_lane_whole_frame() {
        let mut opt = host_session(host_cfg(FecScheme::Gf16, 20, true));
        let mut refr = host_session(host_cfg(FecScheme::Gf16, 20, true));
        // A lane whose worker has already exited: both far ends dropped, so `send` fails
        // immediately and hands the job (with the frame's back half) back.
        let (to_worker, jobs) = std::sync::mpsc::sync_channel::<SealJob>(1);
        let (done_tx, from_worker) = std::sync::mpsc::sync_channel::<SealJob>(1);
        drop(jobs);
        drop(done_tx);
        opt.seal_lane = Some(SealLane {
            to_worker,
            from_worker,
        });
        let frame = pattern(20000); // > TWO_LANE_MIN_PACKETS wire packets → takes the split path
        let got = opt.seal_frame(&frame, 7, 0).unwrap();
        let want = seal_via_wrapper(&mut refr, &frame, 7, 0);
        assert_eq!(got, want, "fallback must seal the whole frame, not half");
        assert!(
            opt.seal_lane.is_none(),
            "the dead lane must be dropped, not retried forever"
        );
        // The next large frame respawns a fresh, working lane.
        opt.reclaim_wires(got);
        let got2 = opt.seal_frame(&frame, 8, 1).unwrap();
        let want2 = seal_via_wrapper(&mut refr, &frame, 8, 1);
        assert_eq!(got2, want2);
        assert!(
            opt.seal_lane.is_some(),
            "a fresh lane respawns on the next large frame"
        );
    }

    /// Partial delivery (plan §4.4): a chunk-aligned frame that loses shards past FEC's
    /// reach is DELIVERED once it ages out — `complete: false`, received shards at their
    /// exact offsets, missing ranges zero-filled — instead of silently dropping. Plain
    /// AUs (no flag) keep the drop behavior even with the opt-in enabled.
    #[test]
    fn partial_delivery_of_chunk_aligned_frames() {
        use crate::packet::USER_FLAG_CHUNK_ALIGNED;
        let mk = |role| Config {
            role,
            phase: ProtocolPhase::P2Punktfunk,
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 0, // no parity — any drop leaves a hole
                max_data_per_block: 64,
            },
            shard_payload: 1024,
            max_frame_bytes: 8 * 1024 * 1024,
            encrypt: false,
            key: SessionKey::Aes128Gcm([0u8; 16]),
            salt: [0u8; 4],
            loopback_drop_period: 0,
        };
        // Drop exactly one datagram (the 3rd) out of the first frame's shards.
        let (h, c) = crate::transport::loopback_pair(3, 1);
        let mut host = Session::new(mk(Role::Host), Box::new(h)).unwrap();
        let mut client = Session::new(mk(Role::Client), Box::new(c)).unwrap();
        client.set_deliver_partial_frames(true);

        // 8 shards of chunk-aligned payload.
        let frame = pattern(8 * 1024);
        host.submit_frame(&frame, 1_000, USER_FLAG_CHUNK_ALIGNED)
            .unwrap();
        // The incomplete frame ages out on the HARD index window — push enough newer
        // (complete) frames past it. Collect everything the client emits.
        let mut got_partial = None;
        let mut completes = 0;
        for i in 0..80u64 {
            let filler = pattern(1024);
            host.submit_frame(&filler, 2_000 + i, USER_FLAG_CHUNK_ALIGNED)
                .unwrap();
            loop {
                match client.poll_frame() {
                    Ok(f) if !f.complete => got_partial = Some(f),
                    Ok(_) => completes += 1,
                    Err(PunktfunkError::NoFrame) => break,
                    Err(e) => panic!("unexpected: {e}"),
                }
            }
        }
        let p = got_partial.expect("the lossy frame must be delivered partial");
        assert_eq!(p.pts_ns, 1_000);
        assert_eq!(p.data.len(), frame.len());
        assert!(p.flags & USER_FLAG_CHUNK_ALIGNED != 0);
        // Exactly one 1024-byte shard is zeroed; every other offset matches the original.
        let mut zero_windows = 0;
        for w in 0..8 {
            let win = &p.data[w * 1024..(w + 1) * 1024];
            if win.iter().all(|&b| b == 0) {
                zero_windows += 1;
            } else {
                assert_eq!(win, &frame[w * 1024..(w + 1) * 1024], "window {w} corrupt");
            }
        }
        // loopback_pair(3, _) drops every 3rd datagram, so several of the 8 shards are
        // gone — the exact count depends on phase; what matters is that SOME are zeroed
        // and every survivor is intact.
        assert!(
            (1..8).contains(&zero_windows),
            "dropped shards zero-filled (got {zero_windows})"
        );
        assert!(completes > 40, "surviving filler frames flow normally");

        // Control: WITHOUT the flag the same loss is a plain drop, opt-in or not.
        let (h2, c2) = crate::transport::loopback_pair(3, 1);
        let mut host2 = Session::new(mk(Role::Host), Box::new(h2)).unwrap();
        let mut client2 = Session::new(mk(Role::Client), Box::new(c2)).unwrap();
        client2.set_deliver_partial_frames(true);
        host2.submit_frame(&pattern(8 * 1024), 1_000, 0).unwrap();
        let mut saw_partial = false;
        for i in 0..80u64 {
            host2.submit_frame(&pattern(1024), 2_000 + i, 0).unwrap();
            loop {
                match client2.poll_frame() {
                    Ok(f) => saw_partial |= !f.complete,
                    Err(PunktfunkError::NoFrame) => break,
                    Err(e) => panic!("unexpected: {e}"),
                }
            }
        }
        assert!(
            !saw_partial,
            "unflagged AUs must never be delivered partial"
        );
    }
}
