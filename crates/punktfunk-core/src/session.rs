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
use crate::packet::{Packetizer, Reassembler, ReassemblerLimits};
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
}

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

    /// Host: FEC-protect, packetize, seal, and send one encoded access unit.
    pub fn submit_frame(&mut self, data: &[u8], pts_ns: u64, user_flags: u32) -> Result<()> {
        if self.config.role != Role::Host {
            return Err(PunktfunkError::InvalidArg(
                "submit_frame called on a client session",
            ));
        }
        let packets = self
            .packetizer
            .packetize(data, pts_ns, user_flags, self.coder.as_ref())?;
        StatsCounters::add(&self.stats.frames_submitted, 1);
        for pkt in packets {
            let wire = self.seal_for_wire(&pkt)?;
            StatsCounters::add(&self.stats.packets_sent, 1);
            StatsCounters::add(&self.stats.bytes_sent, wire.len() as u64);
            self.transport.send(&wire)?;
        }
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
        loop {
            let wire = match self.transport.recv()? {
                Some(w) => w,
                None => return Err(PunktfunkError::NoFrame),
            };
            let pkt = match self.open_from_wire(&wire) {
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
        self.transport.send(&wire)?;
        Ok(())
    }
}
