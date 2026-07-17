//! `punktfunk/1` — the native control plane, gated behind the `quic` feature.
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
//! The Welcome carries everything the core negotiates — FEC scheme (including GF(2¹⁶)
//! Leopard, which GameStream can't express), shard sizing, crypto key/salt — so the data
//! plane is exactly the hardened core `Session`.
//!
//! Transport security: the host presents a long-lived self-signed certificate
//! ([`endpoint::server_with_identity`]) and the client pins its SHA-256 fingerprint
//! ([`endpoint::client_pinned`]; no pin = trust-on-first-use, with the observed fingerprint
//! reported back for persisting). The data plane adds AES-GCM on top.
//! All integers little-endian; every message is `u16 length || payload`.
//!
//! Split by concern (networking-audit deferred plan §3 — a pure move): [`msgs`] the
//! handshake + typed control messages, [`pake`] the pairing SPAKE2, [`datagram`] the
//! 0xC9–0xCF plane codecs, [`io`] framed stream IO, [`clock`] skew estimation + mid-stream
//! re-sync, [`endpoint`] the quinn constructors. Every item is re-exported here, so all
//! existing `crate::quic::X` paths compile unchanged.

/// Protocol magic + version, first bytes of the positional handshake (Hello/Welcome/Start).
pub const MAGIC: &[u8; 4] = b"PKF1";

/// Magic for typed post-handshake / pairing control messages. A distinct magic keeps the
/// typed namespace disjoint from the positional handshake: a `Hello` (whose abi_version
/// byte sits where a type byte would) can never be misparsed as a control message, and
/// vice-versa, regardless of field values.
pub const CTL_MAGIC: &[u8; 4] = b"PKFc";

mod caps;
mod clock;
mod control;
mod datagram;
mod handshake;
mod pairing;

/// quinn endpoint constructors. Host: self-signed identity (fresh, or persisted PEMs via
/// [`endpoint::server_with_identity`]). Client: fingerprint pinning / TOFU via
/// [`endpoint::client_pinned`] ([`endpoint::client_insecure`] is the no-pin special case).
pub mod endpoint;

/// Async framed-message IO over a quinn stream (`u16 LE length || payload`).
pub mod io;

/// Per-transfer clipboard fetch bi-streams (`PKFs` magic + kind byte, then request/response). The
/// transport half of the shared clipboard; wire codecs are in [`control`], state lives per side.
pub mod clipstream;

/// SPAKE2 over Ed25519 for the pairing ceremony. The two roles use the asymmetric flow so
/// the identities are ordered; each side binds **both** certificate fingerprints as the
/// SPAKE2 identities, so the derived key only matches when client and host agree on the PIN
/// *and* saw the same two certificates (a MITM, presenting different certs to each leg,
/// cannot reach a shared key).
pub mod pake;

pub use caps::*;
pub use clock::*;
pub use control::*;
pub use datagram::*;
pub use handshake::*;
pub use pairing::*;

// Typed rejection close codes + [`RejectReason`] live in `crate::reject` (ungated — the
// error enum references them even in `quic`-less builds) and are re-exported here so the
// wire vocabulary stays browsable next to QUIT/APP_EXITED.
pub use crate::reject::*;

#[cfg(test)]
mod tests;
