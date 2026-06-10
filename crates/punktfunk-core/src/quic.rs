//! `punktfunk/1` — the native control plane (M3), gated behind the `quic` feature.
//!
//! GameStream is punktfunk's compatibility layer; this is the start of its own protocol. A QUIC
//! connection (quinn, tokio — control plane only, never the per-frame path) carries a
//! length-prefixed binary handshake on one bidirectional stream:
//!
//! ```text
//!   client → host  Hello   { abi_version }
//!   host → client  Welcome { abi_version, session: full data-plane Config + mode + UDP port }
//!   client → host  Start   { client_udp_port }
//! ```
//!
//! after which both sides bring up a [`crate::session::Session`] over a plain
//! [`UdpTransport`](crate::transport::udp) (native threads, no async) and the host streams.
//! The Welcome carries everything the M1 core negotiates — FEC scheme (including GF(2¹⁶)
//! Leopard, which GameStream can't express), shard sizing, crypto key/salt — so the data
//! plane is exactly the hardened M1 `Session`.
//!
//! Transport security: the host presents a long-lived self-signed certificate
//! ([`endpoint::server_with_identity`]) and the client pins its SHA-256 fingerprint
//! ([`endpoint::client_pinned`]; no pin = trust-on-first-use, with the observed fingerprint
//! reported back for persisting). The data plane adds AES-GCM on top.
//! All integers little-endian; every message is `u16 length || payload`.

use crate::config::{CompositorPref, Config, FecConfig, FecScheme, Mode, ProtocolPhase, Role};
use crate::error::{PunktfunkError, Result};

/// Protocol magic + version, first bytes of the positional handshake (Hello/Welcome/Start).
pub const MAGIC: &[u8; 4] = b"PKF1";

/// Magic for typed post-handshake / pairing control messages. A distinct magic keeps the
/// typed namespace disjoint from the positional handshake: a `Hello` (whose abi_version
/// byte sits where a type byte would) can never be misparsed as a control message, and
/// vice-versa, regardless of field values.
pub const CTL_MAGIC: &[u8; 4] = b"PKFc";

/// `client → host`: open the session, requesting a display mode (the host creates its
/// virtual output at exactly this size/refresh — native resolution end to end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub abi_version: u32,
    pub mode: Mode,
    /// Which compositor the client would like the host to drive (`Auto` = host decides). The
    /// host honors it only if that backend is available, else falls back and reports the real
    /// choice in [`Welcome::compositor`]. Appended to the wire form — omitted by older clients
    /// (decodes to `Auto`).
    pub compositor: CompositorPref,
}

/// `host → client`: the complete session offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub abi_version: u32,
    /// Host UDP port for the data plane.
    pub udp_port: u16,
    pub mode: Mode,
    pub fec: FecConfig,
    pub shard_payload: u16,
    pub encrypt: bool,
    pub key: [u8; 16],
    pub salt: [u8; 4],
    /// Seed/testing: how many frames the host will send (0 = unbounded).
    pub frames: u32,
    /// The compositor the host actually resolved for this session (the client's
    /// [`Hello::compositor`] preference if available, else the host's auto-detected choice).
    /// Appended to the wire form — `Auto` when an older host omitted it (i.e. "unknown").
    pub compositor: CompositorPref,
}

/// `client → host`: data plane is bound, begin streaming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    pub client_udp_port: u16,
}

/// `client → host`, any time after [`Start`]: switch the session to a new display mode
/// (window resized, refresh changed) without reconnecting. The host answers with
/// [`Reconfigured`]; on acceptance it rebuilds its virtual output + encoder at the new
/// mode and the stream continues over the unchanged data plane — the first new-mode frame
/// is an IDR with in-band parameter sets, which is all a decoder needs to follow.
///
/// Post-handshake messages carry a type byte after the magic (the handshake itself is
/// positional and stays untyped for wire compatibility).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconfigure {
    pub mode: Mode,
}

/// `host → client`: answer to [`Reconfigure`]. `accepted = false` means the requested
/// mode was rejected (e.g. exceeds encoder limits) and the session continues at `mode`
/// (the still-active one); `true` means `mode` is now being switched to live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconfigured {
    pub accepted: bool,
    pub mode: Mode,
}

/// Type byte of [`Reconfigure`] (first byte after the magic).
pub const MSG_RECONFIGURE: u8 = 0x01;
/// Type byte of [`Reconfigured`].
pub const MSG_RECONFIGURED: u8 = 0x02;

// ---------------------------------------------------------------------------------------------
// Pairing ceremony (typed control messages): instead of a session Hello, a client may open
// the control stream with PairRequest. The host shows a short PIN out-of-band (log/UI); the
// user types it into the client.
//
// Trust is established by **SPAKE2** (a balanced PAKE), NOT a hash of the PIN. SPAKE2 turns
// the low-entropy PIN into a high-entropy shared key via a Diffie-Hellman exchange; the only
// thing an active man-in-the-middle who terminates the (TOFU) ceremony learns is whether a
// single PIN guess was right — there is no transcript value that reveals the PIN to an
// *offline* dictionary search (the fatal flaw of an HMAC-of-PIN proof over a 4-digit space).
// Both peers' certificate fingerprints are bound in as the SPAKE2 identities, so the
// established key — and the key-confirmation MACs derived from it — only agree when both
// sides saw the same two certificates. After mutual key confirmation the host persists the
// client's fingerprint and the client pins the host's.
// ---------------------------------------------------------------------------------------------

/// Type byte of [`PairRequest`].
pub const MSG_PAIR_REQUEST: u8 = 0x10;
/// Type byte of [`PairChallenge`].
pub const MSG_PAIR_CHALLENGE: u8 = 0x11;
/// Type byte of [`PairProof`].
pub const MSG_PAIR_PROOF: u8 = 0x12;
/// Type byte of [`PairResult`].
pub const MSG_PAIR_RESULT: u8 = 0x13;

/// `client → host`: begin pairing. `name` is the human label the host stores (≤64 bytes
/// UTF-8); `spake_a` is the client's SPAKE2 message (see [`SpakeRole::start`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequest {
    pub name: String,
    pub spake_a: Vec<u8>,
}

/// `host → client`: the host's SPAKE2 message + its key-confirmation MAC. The client
/// finishes SPAKE2, verifies `confirm` (proving the host derived the same key, i.e. knows
/// the PIN and saw the same certs), then sends its own confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairChallenge {
    pub spake_b: Vec<u8>,
    pub confirm: [u8; 32],
}

/// `client → host`: the client's key-confirmation MAC (its single proof attempt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairProof {
    pub confirm: [u8; 32],
}

/// `host → client`: ceremony outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairResult {
    pub ok: bool,
}

/// A length-prefixed (u16 LE) byte field within a control message.
fn put_bytes(b: &mut Vec<u8>, x: &[u8]) {
    b.extend_from_slice(&(x.len() as u16).to_le_bytes());
    b.extend_from_slice(x);
}

/// Read a length-prefixed field at `off`, returning the bytes and the next offset.
fn get_bytes(b: &[u8], off: usize) -> Result<(&[u8], usize)> {
    if off + 2 > b.len() {
        return Err(PunktfunkError::InvalidArg("truncated field"));
    }
    let n = u16::from_le_bytes([b[off], b[off + 1]]) as usize;
    let start = off + 2;
    if start + n > b.len() {
        return Err(PunktfunkError::InvalidArg("field overruns message"));
    }
    Ok((&b[start..start + n], start + n))
}

impl PairRequest {
    pub fn encode(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let n = name.len().min(64);
        let mut b = Vec::with_capacity(8 + n + self.spake_a.len());
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_REQUEST);
        b.push(n as u8);
        b.extend_from_slice(&name[..n]);
        put_bytes(&mut b, &self.spake_a);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairRequest> {
        if b.len() < 6 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_REQUEST {
            return Err(PunktfunkError::InvalidArg("bad PairRequest"));
        }
        let n = b[5] as usize;
        if n > 64 || b.len() < 6 + n {
            return Err(PunktfunkError::InvalidArg("bad PairRequest name"));
        }
        let name = String::from_utf8_lossy(&b[6..6 + n]).into_owned();
        let (spake_a, end) = get_bytes(b, 6 + n)?;
        if end != b.len() {
            return Err(PunktfunkError::InvalidArg("trailing bytes"));
        }
        Ok(PairRequest {
            name,
            spake_a: spake_a.to_vec(),
        })
    }
}

impl PairChallenge {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(7 + self.spake_b.len() + 32);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_CHALLENGE);
        put_bytes(&mut b, &self.spake_b);
        b.extend_from_slice(&self.confirm);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairChallenge> {
        if b.len() < 5 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_CHALLENGE {
            return Err(PunktfunkError::InvalidArg("bad PairChallenge"));
        }
        let (spake_b, end) = get_bytes(b, 5)?;
        if end + 32 != b.len() {
            return Err(PunktfunkError::InvalidArg("bad PairChallenge confirm"));
        }
        let mut confirm = [0u8; 32];
        confirm.copy_from_slice(&b[end..end + 32]);
        Ok(PairChallenge {
            spake_b: spake_b.to_vec(),
            confirm,
        })
    }
}

impl PairProof {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(37);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_PROOF);
        b.extend_from_slice(&self.confirm);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairProof> {
        if b.len() != 37 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_PROOF {
            return Err(PunktfunkError::InvalidArg("bad PairProof"));
        }
        let mut confirm = [0u8; 32];
        confirm.copy_from_slice(&b[5..37]);
        Ok(PairProof { confirm })
    }
}

impl PairResult {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(6);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_RESULT);
        b.push(self.ok as u8);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairResult> {
        if b.len() != 6 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_RESULT {
            return Err(PunktfunkError::InvalidArg("bad PairResult"));
        }
        Ok(PairResult { ok: b[5] != 0 })
    }
}

/// SPAKE2 over Ed25519 for the pairing ceremony. The two roles use the asymmetric flow so
/// the identities are ordered; each side binds **both** certificate fingerprints as the
/// SPAKE2 identities, so the derived key only matches when client and host agree on the PIN
/// *and* saw the same two certificates (a MITM, presenting different certs to each leg,
/// cannot reach a shared key).
pub mod pake {
    use super::*;
    use hmac::{Hmac, Mac};
    use spake2::{Ed25519Group, Identity, Password, Spake2};

    /// In-progress SPAKE2 state plus the identity transcript for key confirmation.
    pub struct PairingPake {
        state: Spake2<Ed25519Group>,
        transcript: Vec<u8>,
    }

    /// Start the exchange. `client_fp`/`host_fp` are the two certificate fingerprints (the
    /// client passes what it observed via TOFU; the host passes its own + the client's
    /// presented cert). Returns the state and this side's outbound SPAKE2 message.
    pub fn start(
        is_client: bool,
        pin: &str,
        client_fp: &[u8; 32],
        host_fp: &[u8; 32],
    ) -> (PairingPake, Vec<u8>) {
        let pw = Password::new(pin.as_bytes());
        let id_client = Identity::new(client_fp);
        let id_host = Identity::new(host_fp);
        let (state, msg) = if is_client {
            Spake2::<Ed25519Group>::start_a(&pw, &id_client, &id_host)
        } else {
            Spake2::<Ed25519Group>::start_b(&pw, &id_client, &id_host)
        };
        let mut transcript = Vec::with_capacity(64);
        transcript.extend_from_slice(client_fp);
        transcript.extend_from_slice(host_fp);
        (PairingPake { state, transcript }, msg)
    }

    /// Key confirmation MAC for one direction (`label` distinguishes host vs client), keyed
    /// by the SPAKE2 shared key and bound to the fingerprint transcript.
    fn confirm(key: &[u8], label: &[u8], transcript: &[u8]) -> [u8; 32] {
        let mut mac =
            <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("hmac takes any key length");
        mac.update(label);
        mac.update(transcript);
        mac.finalize().into_bytes().into()
    }

    /// `Hmac` verification is constant-time via `ct_eq` in the underlying crate; we compare
    /// our recomputed tag the same way.
    fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
        a.iter()
            .zip(b.iter())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
    }

    /// Confirmation tags both sides expect, given the agreed SPAKE2 key.
    pub struct Confirmations {
        /// MAC the host sends (client verifies).
        pub host: [u8; 32],
        /// MAC the client sends (host verifies).
        pub client: [u8; 32],
    }

    impl PairingPake {
        /// Finish SPAKE2 with the peer's message → the pair of confirmation tags. `Err` if
        /// the peer's message is malformed (a wrong PIN does NOT error here — it yields a
        /// *different* key, so the confirmation MACs simply won't match).
        pub fn finish(self, peer_msg: &[u8]) -> Result<Confirmations> {
            let key = self
                .state
                .finish(peer_msg)
                .map_err(|_| PunktfunkError::Crypto)?;
            Ok(Confirmations {
                host: confirm(&key, b"punktfunk-pair-host", &self.transcript),
                client: confirm(&key, b"punktfunk-pair-client", &self.transcript),
            })
        }
    }

    /// Constant-time tag comparison for the confirmation step.
    pub fn verify(expected: &[u8; 32], got: &[u8; 32]) -> bool {
        ct_eq(expected, got)
    }
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(21);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.abi_version.to_le_bytes());
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b.push(self.compositor.to_u8()); // appended at offset 20 — older hosts read [0..20] and skip it
        b
    }

    pub fn decode(b: &[u8]) -> Result<Hello> {
        if b.len() < 20 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Hello"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Ok(Hello {
            abi_version: u32at(4),
            mode: Mode {
                width: u32at(8),
                height: u32at(12),
                refresh_hz: u32at(16),
            },
            // Optional trailing byte — an older client that omits it requests `Auto`.
            compositor: b
                .get(20)
                .map(|&v| CompositorPref::from_u8(v))
                .unwrap_or_default(),
        })
    }
}

impl Welcome {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.abi_version.to_le_bytes());
        b.extend_from_slice(&self.udp_port.to_le_bytes());
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b.push(match self.fec.scheme {
            FecScheme::Gf8 => 0,
            FecScheme::Gf16 => 1,
        });
        b.push(self.fec.fec_percent);
        b.extend_from_slice(&self.fec.max_data_per_block.to_le_bytes());
        b.extend_from_slice(&self.shard_payload.to_le_bytes());
        b.push(self.encrypt as u8);
        b.extend_from_slice(&self.key);
        b.extend_from_slice(&self.salt);
        b.extend_from_slice(&self.frames.to_le_bytes());
        b.push(self.compositor.to_u8()); // appended at offset 53 — older clients read [0..53] and skip it
        b
    }

    pub fn decode(b: &[u8]) -> Result<Welcome> {
        // Layout (LE): magic[0..4] abi[4..8] port[8..10] w[10..14] h[14..18] hz[18..22]
        // scheme[22] pct[23] max_data[24..26] shard[26..28] encrypt[28] key[29..45]
        // salt[45..49] frames[49..53] compositor[53] (optional trailing byte).
        if b.len() < 53 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Welcome"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let mut key = [0u8; 16];
        key.copy_from_slice(&b[29..45]);
        let mut salt = [0u8; 4];
        salt.copy_from_slice(&b[45..49]);
        Ok(Welcome {
            abi_version: u32at(4),
            udp_port: u16at(8),
            mode: Mode {
                width: u32at(10),
                height: u32at(14),
                refresh_hz: u32at(18),
            },
            fec: FecConfig {
                scheme: if b[22] == 1 {
                    FecScheme::Gf16
                } else {
                    FecScheme::Gf8
                },
                fec_percent: b[23],
                max_data_per_block: u16at(24),
            },
            shard_payload: u16at(26),
            encrypt: b[28] != 0,
            key,
            salt,
            frames: u32at(49),
            // Optional trailing byte — an older host that omits it leaves the resolved
            // compositor unknown (`Auto`).
            compositor: b
                .get(53)
                .map(|&v| CompositorPref::from_u8(v))
                .unwrap_or_default(),
        })
    }

    /// Build the data-plane [`Config`] this offer describes (for `role`).
    pub fn session_config(&self, role: Role) -> Config {
        let mut c = Config::p1_defaults(role);
        c.phase = ProtocolPhase::P1GameStream; // wire phase id pending the P2 packet rev
        c.fec = self.fec;
        c.shard_payload = self.shard_payload as usize;
        c.encrypt = self.encrypt;
        c.key = self.key;
        c.salt = self.salt;
        c
    }
}

impl Start {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(6);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.client_udp_port.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Start> {
        if b.len() < 6 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Start"));
        }
        Ok(Start {
            client_udp_port: u16::from_le_bytes([b[4], b[5]]),
        })
    }
}

impl Reconfigure {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] w[5..9] h[9..13] hz[13..17]
        let mut b = Vec::with_capacity(17);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_RECONFIGURE);
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Reconfigure> {
        if b.len() != 17 || &b[0..4] != CTL_MAGIC || b[4] != MSG_RECONFIGURE {
            return Err(PunktfunkError::InvalidArg("bad Reconfigure"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Ok(Reconfigure {
            mode: Mode {
                width: u32at(5),
                height: u32at(9),
                refresh_hz: u32at(13),
            },
        })
    }
}

impl Reconfigured {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] accepted[5] w[6..10] h[10..14] hz[14..18]
        let mut b = Vec::with_capacity(18);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_RECONFIGURED);
        b.push(self.accepted as u8);
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Reconfigured> {
        if b.len() != 18 || &b[0..4] != CTL_MAGIC || b[4] != MSG_RECONFIGURED {
            return Err(PunktfunkError::InvalidArg("bad Reconfigured"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Ok(Reconfigured {
            accepted: b[5] != 0,
            mode: Mode {
                width: u32at(6),
                height: u32at(10),
                refresh_hz: u32at(14),
            },
        })
    }
}

/// Frame a message for the control stream: `u16 LE length || payload`.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(2 + payload.len());
    b.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    b.extend_from_slice(payload);
    b
}

/// Datagram wire tags. Video rides UDP; everything low-rate rides QUIC datagrams,
/// demultiplexed by the first byte: input = [`crate::input::INPUT_MAGIC`] (0xC8),
/// audio = [`AUDIO_MAGIC`], rumble = [`RUMBLE_MAGIC`].
pub const AUDIO_MAGIC: u8 = 0xC9;
pub const RUMBLE_MAGIC: u8 = 0xCA;

/// Audio datagram, host → client: `[0xC9][u32 seq LE][u64 pts_ns LE][opus payload]`.
/// One Opus frame per datagram (5 ms — well under any MTU); QUIC already encrypts.
pub fn encode_audio_datagram(seq: u32, pts_ns: u64, opus: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(13 + opus.len());
    b.push(AUDIO_MAGIC);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    b.extend_from_slice(opus);
    b
}

/// Parse an audio datagram → `(seq, pts_ns, opus payload)`. `None` on bad tag/length.
pub fn decode_audio_datagram(b: &[u8]) -> Option<(u32, u64, &[u8])> {
    if b.len() < 13 || b[0] != AUDIO_MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes(b[1..5].try_into().unwrap());
    let pts_ns = u64::from_le_bytes(b[5..13].try_into().unwrap());
    Some((seq, pts_ns, &b[13..]))
}

/// Rumble datagram, host → client: `[0xCA][u16 pad LE][u16 low LE][u16 high LE]`.
/// Force-feedback state for pad `pad` (0xFFFF amplitudes, 0/0 = stop).
pub fn encode_rumble_datagram(pad: u16, low: u16, high: u16) -> [u8; 7] {
    let mut b = [0u8; 7];
    b[0] = RUMBLE_MAGIC;
    b[1..3].copy_from_slice(&pad.to_le_bytes());
    b[3..5].copy_from_slice(&low.to_le_bytes());
    b[5..7].copy_from_slice(&high.to_le_bytes());
    b
}

/// Parse a rumble datagram → `(pad, low, high)`. `None` on bad tag/length.
pub fn decode_rumble_datagram(b: &[u8]) -> Option<(u16, u16, u16)> {
    if b.len() < 7 || b[0] != RUMBLE_MAGIC {
        return None;
    }
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    Some((u16at(1), u16at(3), u16at(5)))
}

/// Async framed-message IO over a quinn stream (`u16 LE length || payload`).
pub mod io {
    /// Read one framed message (bounded at 64 KiB — control messages are tiny).
    pub async fn read_msg(recv: &mut quinn::RecvStream) -> std::io::Result<Vec<u8>> {
        let mut len = [0u8; 2];
        recv.read_exact(&mut len)
            .await
            .map_err(std::io::Error::other)?;
        let n = u16::from_le_bytes(len) as usize;
        let mut buf = vec![0u8; n];
        recv.read_exact(&mut buf)
            .await
            .map_err(std::io::Error::other)?;
        Ok(buf)
    }

    /// Write one framed message.
    pub async fn write_msg(send: &mut quinn::SendStream, payload: &[u8]) -> std::io::Result<()> {
        send.write_all(&super::frame(payload))
            .await
            .map_err(std::io::Error::other)
    }
}

/// quinn endpoint constructors. Host: self-signed identity (fresh, or persisted PEMs via
/// [`endpoint::server_with_identity`]). Client: fingerprint pinning / TOFU via
/// [`endpoint::client_pinned`] ([`endpoint::client_insecure`] is the no-pin special case).
pub mod endpoint {
    use std::sync::{Arc, Mutex};

    /// Server endpoint with a fresh self-signed certificate (tests/dev — production hosts
    /// persist an identity and use [`server_with_identity`] so clients can pin it).
    pub fn server(addr: std::net::SocketAddr) -> anyhow_result::Result<quinn::Endpoint> {
        let cert = rcgen::generate_simple_self_signed(vec!["punktfunk".into()])
            .map_err(|e| anyhow_result::Error::msg(format!("self-signed cert: {e}")))?;
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
        let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        server_from_der(cert_der, key_der.into(), addr)
    }

    /// Server endpoint from a persisted PEM identity (certificate + PKCS#8 private key) —
    /// the host's long-lived self-signed cert, so the fingerprint clients pin is stable
    /// across restarts.
    pub fn server_with_identity(
        addr: std::net::SocketAddr,
        cert_pem: &str,
        key_pem: &str,
    ) -> anyhow_result::Result<quinn::Endpoint> {
        use rustls::pki_types::pem::PemObject;
        let cert_der = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|e| anyhow_result::Error::msg(format!("cert pem: {e}")))?;
        let key_der = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
            .map_err(|e| anyhow_result::Error::msg(format!("key pem: {e}")))?;
        server_from_der(cert_der, key_der, addr)
    }

    fn server_from_der(
        cert_der: rustls::pki_types::CertificateDer<'static>,
        key_der: rustls::pki_types::PrivateKeyDer<'static>,
        addr: std::net::SocketAddr,
    ) -> anyhow_result::Result<quinn::Endpoint> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Client auth is OFFERED but optional: a client that presents its self-signed
        // identity is fingerprinted post-handshake (pairing / --require-pairing checks);
        // one that presents none still connects (and is rejected at the app layer when
        // pairing is required).
        let rustls_cfg = rustls::ServerConfig::builder()
            .with_client_cert_verifier(Arc::new(AcceptAnyClientCert))
            .with_single_cert(vec![cert_der], key_der)
            .map_err(|e| anyhow_result::Error::msg(format!("server config: {e}")))?;
        let quic_cfg = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)
            .map_err(|e| anyhow_result::Error::msg(format!("quic server config: {e}")))?;
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_cfg));
        Ok(quinn::Endpoint::server(server_config, addr)?)
    }

    /// Generate a fresh self-signed identity (certificate + PKCS#8 key, both PEM) — what a
    /// client persists once and presents on every connect so hosts can recognize it.
    pub fn generate_identity() -> anyhow_result::Result<(String, String)> {
        let cert = rcgen::generate_simple_self_signed(vec!["punktfunk-client".into()])
            .map_err(|e| anyhow_result::Error::msg(format!("self-signed cert: {e}")))?;
        Ok((cert.cert.pem(), cert.key_pair.serialize_pem()))
    }

    /// Fingerprint of the client certificate a connection presented (host side), if any.
    pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<[u8; 32]> {
        let identity = conn.peer_identity()?;
        let certs = identity
            .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
            .ok()?;
        certs.first().map(|c| cert_fingerprint(c.as_ref()))
    }

    /// SHA-256 of a certificate's DER encoding — the fingerprint clients pin.
    pub fn cert_fingerprint(cert_der: &[u8]) -> [u8; 32] {
        use sha2::Digest;
        sha2::Sha256::digest(cert_der).into()
    }

    /// Fingerprint of a PEM-encoded certificate (what a host logs/shows for pairing UX —
    /// must match what the client's verifier computes from the DER on the wire).
    pub fn fingerprint_of_pem(cert_pem: &str) -> anyhow_result::Result<[u8; 32]> {
        use rustls::pki_types::pem::PemObject;
        let der = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|e| anyhow_result::Error::msg(format!("cert pem: {e}")))?;
        Ok(cert_fingerprint(der.as_ref()))
    }

    /// Client endpoint that skips certificate verification (TOFU bootstrap — read the
    /// observed fingerprint off the slot and pin it on the next connect).
    pub fn client_insecure() -> anyhow_result::Result<quinn::Endpoint> {
        client_pinned(None).0
    }

    /// What [`client_pinned`] returns: the endpoint plus the slot the verifier writes the
    /// observed host fingerprint into during the handshake.
    pub type PinnedClient = (
        anyhow_result::Result<quinn::Endpoint>,
        Arc<Mutex<Option<[u8; 32]>>>,
    );

    /// Client endpoint that verifies the host by certificate fingerprint.
    ///
    /// `pin = Some(sha256)` rejects any host whose leaf cert doesn't hash to `sha256`;
    /// `None` accepts any (trust-on-first-use). Either way the observed fingerprint is
    /// written to the returned slot during the handshake, so a TOFU caller can persist it.
    pub fn client_pinned(pin: Option<[u8; 32]>) -> PinnedClient {
        client_pinned_with_identity(pin, None)
    }

    /// [`client_pinned`], additionally presenting a client identity (PEM cert + PKCS#8
    /// key) via TLS client auth — how a paired client identifies itself to the host.
    pub fn client_pinned_with_identity(
        pin: Option<[u8; 32]>,
        identity: Option<(&str, &str)>,
    ) -> PinnedClient {
        let observed = Arc::new(Mutex::new(None));
        let ep = (|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let builder = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PinVerify {
                    pin,
                    observed: observed.clone(),
                }));
            let rustls_cfg = match identity {
                None => builder.with_no_client_auth(),
                Some((cert_pem, key_pem)) => {
                    use rustls::pki_types::pem::PemObject;
                    let cert =
                        rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
                            .map_err(|e| {
                                anyhow_result::Error::msg(format!("client cert pem: {e}"))
                            })?;
                    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
                        .map_err(|e| anyhow_result::Error::msg(format!("client key pem: {e}")))?;
                    builder
                        .with_client_auth_cert(vec![cert], key)
                        .map_err(|e| anyhow_result::Error::msg(format!("client auth: {e}")))?
                }
            };
            let quic_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)
                .map_err(|e| anyhow_result::Error::msg(format!("quic client config: {e}")))?;
            let mut ep = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())?;
            ep.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_cfg)));
            Ok(ep)
        })();
        (ep, observed)
    }

    /// Minimal error plumbing without pulling anyhow into punktfunk-core's public API.
    pub mod anyhow_result {
        pub type Result<T> = std::result::Result<T, Error>;
        #[derive(Debug)]
        pub struct Error(String);
        impl Error {
            pub fn msg(s: String) -> Self {
                Error(s)
            }
        }
        impl std::fmt::Display for Error {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for Error {}
        impl From<std::io::Error> for Error {
            fn from(e: std::io::Error) -> Self {
                Error(e.to_string())
            }
        }
    }

    /// Fingerprint-pinning verifier: trust is the SHA-256 of the host's (self-signed) leaf
    /// cert, not a CA chain. With no pin it accepts any cert (TOFU) but still records what
    /// it saw, so the embedder can persist the fingerprint and pin it from then on.
    /// Server-side client-cert verifier: accept any (self-signed) client certificate but
    /// verify the handshake signature for real — possession of the presented cert's key is
    /// what makes the post-handshake fingerprint ([`peer_fingerprint`]) meaningful.
    /// Authorization (is this fingerprint paired?) happens at the application layer.
    #[derive(Debug)]
    struct AcceptAnyClientCert;

    impl rustls::server::danger::ClientCertVerifier for AcceptAnyClientCert {
        fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
            &[]
        }

        fn client_auth_mandatory(&self) -> bool {
            false // unpaired/legacy clients still connect; gating is per-feature
        }

        fn verify_client_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _now: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error>
        {
            Ok(rustls::server::danger::ClientCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    #[derive(Debug)]
    struct PinVerify {
        pin: Option<[u8; 32]>,
        observed: Arc<Mutex<Option<[u8; 32]>>>,
    }

    impl rustls::client::danger::ServerCertVerifier for PinVerify {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error>
        {
            let fp = cert_fingerprint(end_entity.as_ref());
            *self.observed.lock().unwrap() = Some(fp);
            if let Some(expected) = self.pin {
                if fp != expected {
                    return Err(rustls::Error::InvalidCertificate(
                        rustls::CertificateError::ApplicationVerificationFailure,
                    ));
                }
            }
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        // The handshake signatures MUST be verified for real even though we pin the cert:
        // CertificateVerify is what proves the peer *holds the pinned cert's private key* —
        // skip it and an active MITM can replay the host's (public) certificate, match the
        // pin, and complete the handshake with its own key.
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_roundtrip() {
        let w = Welcome {
            abi_version: 1,
            udp_port: 9999,
            mode: Mode {
                width: 2560,
                height: 1440,
                refresh_hz: 240,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [7u8; 16],
            salt: [1, 2, 3, 4],
            frames: 600,
            compositor: CompositorPref::Gamescope,
        };
        assert_eq!(Welcome::decode(&w.encode()).unwrap(), w);
    }

    #[test]
    fn hello_start_roundtrip() {
        let h = Hello {
            abi_version: 1,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 120,
            },
            compositor: CompositorPref::Kwin,
        };
        assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
        let s = Start {
            client_udp_port: 1234,
        };
        assert_eq!(Start::decode(&s.encode()).unwrap(), s);
    }

    #[test]
    fn compositor_pref_wire_and_names() {
        for p in [
            CompositorPref::Auto,
            CompositorPref::Kwin,
            CompositorPref::Wlroots,
            CompositorPref::Mutter,
            CompositorPref::Gamescope,
        ] {
            assert_eq!(CompositorPref::from_u8(p.to_u8()), p);
            assert_eq!(CompositorPref::from_name(p.as_str()), Some(p));
        }
        // Aliases + unknowns.
        assert_eq!(CompositorPref::from_name("KDE"), Some(CompositorPref::Kwin));
        assert_eq!(
            CompositorPref::from_name("sway"),
            Some(CompositorPref::Wlroots)
        );
        assert_eq!(CompositorPref::from_name("nope"), None);
        // Unknown wire byte degrades to Auto (forward-compatible).
        assert_eq!(CompositorPref::from_u8(200), CompositorPref::Auto);
    }

    #[test]
    fn hello_welcome_compositor_back_compat() {
        // A new client/host appends one byte; the field is optional on decode, so a legacy
        // peer's shorter message still decodes (compositor = Auto), and a legacy peer reading a
        // new message ignores the trailing byte. Simulate both directions by truncation.
        let h = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Mutter,
        };
        let enc = h.encode();
        assert_eq!(enc.len(), 21);
        // Legacy (20-byte) Hello → Auto, mode intact.
        let legacy = Hello::decode(&enc[..20]).unwrap();
        assert_eq!(legacy.compositor, CompositorPref::Auto);
        assert_eq!(legacy.mode, h.mode);

        let w = Welcome {
            abi_version: 2,
            udp_port: 7000,
            mode: h.mode,
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [3u8; 16],
            salt: [9, 8, 7, 6],
            frames: 0,
            compositor: CompositorPref::Kwin,
        };
        let wenc = w.encode();
        assert_eq!(wenc.len(), 54);
        let legacy_w = Welcome::decode(&wenc[..53]).unwrap();
        assert_eq!(legacy_w.compositor, CompositorPref::Auto);
        assert_eq!(legacy_w.frames, 0);
        assert_eq!(legacy_w.key, w.key);
    }

    #[test]
    fn reconfigure_roundtrip() {
        let rq = Reconfigure {
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 144,
            },
        };
        assert_eq!(Reconfigure::decode(&rq.encode()).unwrap(), rq);
        for accepted in [true, false] {
            let rs = Reconfigured {
                accepted,
                mode: rq.mode,
            };
            assert_eq!(Reconfigured::decode(&rs.encode()).unwrap(), rs);
        }
        // The type byte separates the post-handshake messages from each other.
        assert!(Reconfigure::decode(
            &Reconfigured {
                accepted: true,
                mode: rq.mode
            }
            .encode()
        )
        .is_err());
    }

    #[test]
    fn control_messages_disjoint_from_hello() {
        // A Hello uses MAGIC (PKF1); control messages use CTL_MAGIC (PKFc). No Hello — at
        // any abi_version — can be misparsed as a control message, and vice-versa.
        for abi in [1u32, 2, 16, 0x10, 0x0113, 0x1410] {
            let h = Hello {
                abi_version: abi,
                mode: Mode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60,
                },
                compositor: CompositorPref::Auto,
            }
            .encode();
            assert!(PairRequest::decode(&h).is_err(), "abi {abi} parsed as pair");
            assert!(Reconfigure::decode(&h).is_err());
        }
        // And a PairRequest never parses as a Hello.
        let pr = PairRequest {
            name: "x".into(),
            spake_a: vec![0u8; 33],
        }
        .encode();
        assert!(Hello::decode(&pr).is_err());
    }

    #[test]
    fn pair_messages_roundtrip() {
        let pr = PairRequest {
            name: "Enrico's Mac".into(),
            spake_a: vec![1, 2, 3, 4, 5],
        };
        assert_eq!(PairRequest::decode(&pr.encode()).unwrap(), pr);
        let pc = PairChallenge {
            spake_b: vec![9; 33],
            confirm: [7u8; 32],
        };
        assert_eq!(PairChallenge::decode(&pc.encode()).unwrap(), pc);
        let pp = PairProof { confirm: [3u8; 32] };
        assert_eq!(PairProof::decode(&pp.encode()).unwrap(), pp);
        for ok in [true, false] {
            assert_eq!(
                PairResult::decode(&PairResult { ok }.encode()).unwrap().ok,
                ok
            );
        }
        // Length-exact: a truncated/padded PairProof is rejected.
        let mut bad = pp.encode();
        bad.push(0);
        assert!(PairProof::decode(&bad).is_err());
    }

    #[test]
    fn spake2_pairing_agrees_only_on_matching_pin_and_certs() {
        let cfp = [0x11u8; 32];
        let hfp = [0x22u8; 32];

        // Right PIN, same fingerprint views on both sides → both confirmations agree.
        let (ca, ma) = pake::start(true, "4321", &cfp, &hfp);
        let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
        let a = ca.finish(&mb).unwrap();
        let b = cb.finish(&ma).unwrap();
        assert!(pake::verify(&a.host, &b.host) && pake::verify(&a.client, &b.client));

        // Wrong PIN → different keys → confirmations DON'T match (one online guess wasted).
        let (ca, ma) = pake::start(true, "0000", &cfp, &hfp);
        let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
        let a = ca.finish(&mb).unwrap();
        let b = cb.finish(&ma).unwrap();
        assert!(!pake::verify(&a.client, &b.client));

        // MITM: the two legs saw different host certs → no agreement even with the right PIN.
        let attacker_hfp = [0x33u8; 32];
        let (ca, ma) = pake::start(true, "4321", &cfp, &attacker_hfp);
        let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
        let a = ca.finish(&mb).unwrap();
        let b = cb.finish(&ma).unwrap();
        assert!(!pake::verify(&a.client, &b.client));
    }

    #[test]
    fn audio_datagram_roundtrip() {
        let opus = [0x42u8; 97];
        let d = encode_audio_datagram(7, 1_000_000_123, &opus);
        assert_eq!(d[0], AUDIO_MAGIC);
        let (seq, pts, payload) = decode_audio_datagram(&d).unwrap();
        assert_eq!((seq, pts), (7, 1_000_000_123));
        assert_eq!(payload, opus);
        assert!(decode_audio_datagram(&d[..12]).is_none()); // truncated header
        assert!(decode_audio_datagram(&[0u8; 13]).is_none()); // bad magic

        // Empty payload is legal (DTX) — header-only datagram.
        let header_only = encode_audio_datagram(0, 0, &[]);
        let (_, _, empty) = decode_audio_datagram(&header_only).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn rumble_datagram_roundtrip() {
        let d = encode_rumble_datagram(1, 0x1234, 0xFFFF);
        assert_eq!(d[0], RUMBLE_MAGIC);
        assert_eq!(decode_rumble_datagram(&d), Some((1, 0x1234, 0xFFFF)));
        assert!(decode_rumble_datagram(&d[..6]).is_none());
    }

    #[test]
    fn fingerprint_is_sha256_of_der() {
        // Stable across calls, distinct for distinct certs.
        let a = endpoint::cert_fingerprint(b"cert-a");
        assert_eq!(a, endpoint::cert_fingerprint(b"cert-a"));
        assert_ne!(a, endpoint::cert_fingerprint(b"cert-b"));
    }
}
