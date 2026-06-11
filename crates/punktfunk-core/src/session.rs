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
/// Note: the AEAD layer authenticates each datagram but does **not** provide anti-replay.
/// Video replays are largely absorbed by the reassembler's per-frame dedup, but replayed
/// input events are not yet filtered. A sliding-window replay filter keyed on the
/// authenticated sequence belongs with the pairing/handshake layer (M2); until then,
/// rely on the LAN/VPN transport assumption (plan §1).
pub struct Session {
    config: Config,
    coder: Box<dyn ErasureCoder>,
    crypto: Option<SessionCrypto>,
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
}

/// Datagrams drained per `recvmmsg` syscall on the client (the reused ring's size). At ~125k
/// pkt/s this is ~4k syscalls/s instead of 125k; the buffers cost `RECV_BATCH × RECV_BUF` (~64 KB).
const RECV_BATCH: usize = 32;

impl Session {
    pub fn new(config: Config, transport: Box<dyn Transport>) -> Result<Session> {
        config.validate()?;
        let coder = coder_for(config.fec.scheme);
        let crypto = config
            .encrypt
            .then(|| SessionCrypto::new(&config.key, config.salt, config.role));
        let packetizer = Packetizer::new(&config);
        let reassembler = Reassembler::new(ReassemblerLimits::from_config(&config));
        Ok(Session {
            coder,
            crypto,
            transport,
            packetizer,
            reassembler,
            stats: StatsCounters::default(),
            next_seq: 0,
            recv_scratch: Vec::new(),
            recv_lens: Vec::new(),
            recv_count: 0,
            recv_idx: 0,
            config,
        })
    }

    pub fn role(&self) -> Role {
        self.config.role
    }

    pub fn stats(&self) -> Stats {
        self.stats.snapshot()
    }

    /// Wrap a packet for the wire: when encrypting, prepend the 8-byte big-endian
    /// sequence (the receiver derives the GCM nonce from it) then the ciphertext.
    fn seal_for_wire(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        match &self.crypto {
            Some(c) => {
                let ct = c.seal(seq, packet)?;
                let mut wire = Vec::with_capacity(8 + ct.len());
                wire.extend_from_slice(&seq.to_be_bytes());
                wire.extend_from_slice(&ct);
                Ok(wire)
            }
            None => Ok(packet.to_vec()),
        }
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
        if self.config.role != Role::Host {
            return Err(PunktfunkError::InvalidArg(
                "seal_frame called on a client session",
            ));
        }
        let packets = self
            .packetizer
            .packetize(data, pts_ns, user_flags, self.coder.as_ref())?;
        StatsCounters::add(&self.stats.frames_submitted, 1);
        let mut wires: Vec<Vec<u8>> = Vec::with_capacity(packets.len());
        for pkt in &packets {
            wires.push(self.seal_for_wire(pkt)?);
        }
        let bytes: u64 = wires.iter().map(|w| w.len() as u64).sum();
        StatsCounters::add(&self.stats.packets_sent, wires.len() as u64);
        StatsCounters::add(&self.stats.bytes_sent, bytes);
        Ok(wires)
    }

    /// Host: transmit one chunk of already-[`seal_frame`](Self::seal_frame)ed packets in a single
    /// batched `sendmmsg`, returning how many the kernel accepted. The rest (`packets.len() - n`)
    /// are counted as send-buffer drops. Call once for the whole frame, or per paced chunk.
    pub fn send_sealed(&self, packets: &[&[u8]]) -> Result<usize> {
        let sent = self.transport.send_batch(packets)?;
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
        self.send_sealed(&refs)?;
        Ok(())
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
                self.recv_count = self
                    .transport
                    .recv_batch(&mut self.recv_scratch, &mut self.recv_lens)?;
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
            let pkt = match self.open_from_wire(&self.recv_scratch[i][..len]) {
                Ok(p) => p,
                Err(_) => continue,
            };
            StatsCounters::add(&self.stats.packets_received, 1);
            StatsCounters::add(&self.stats.bytes_received, pkt.len() as u64);
            // The reassembler validates the packet via its parsed header (`magic`),
            // ignoring anything that isn't a well-formed video packet.
            if let Some(frame) = self
                .reassembler
                .push(&pkt, self.coder.as_ref(), &self.stats)?
            {
                StatsCounters::add(&self.stats.frames_completed, 1);
                return Ok(frame);
            }
        }
    }

    /// Client: serialize and send one input event to the host.
    pub fn send_input(&mut self, event: &InputEvent) -> Result<()> {
        if self.config.role != Role::Client {
            return Err(PunktfunkError::InvalidArg(
                "send_input called on a host session",
            ));
        }
        let pkt = event.encode();
        let wire = self.seal_for_wire(&pkt)?;
        StatsCounters::add(&self.stats.packets_sent, 1);
        StatsCounters::add(&self.stats.bytes_sent, wire.len() as u64);
        if !self.transport.send(&wire)? {
            StatsCounters::add(&self.stats.packets_send_dropped, 1);
        }
        Ok(())
    }
}
