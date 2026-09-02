//! `punktfunk/1` — native control plane, behind the `quic` feature.
//!
//! One QUIC bidirectional stream (quinn, tokio — control only, never the
//! per-frame path) carries a length-prefixed handshake:
//!
//! ```text
//!   client → host  Hello   { abi_version }
//!   host → client  Welcome { abi_version, session: Config + mode + UDP port }
//!   client → host  Start   { client_udp_port }
//! ```
//!
//! Both sides then open a [`crate::session::Session`] over
//! [`UdpTransport`](crate::transport::udp) (native threads). Welcome carries
//! the negotiated data-plane config (FEC, shard size, key/salt). The host
//! presents a long-lived self-signed cert ([`endpoint::server_with_identity`]);
//! the client pins its SHA-256 fingerprint ([`endpoint::client_pinned`]; no
//! pin = TOFU). Data-plane AES-GCM sits on top. Integers little-endian; every
//! message is `u16 length || payload`. Submodules re-export here as
//! `crate::quic::X`.

/// Protocol magic + version; first bytes of Hello/Welcome/Start.
pub const MAGIC: &[u8; 4] = b"PKF1";

/// Magic for typed post-handshake / pairing messages. Distinct from [`MAGIC`] so a
/// `Hello` (abi_version where a type byte would sit) cannot parse as control, and
/// vice versa.
pub const CTL_MAGIC: &[u8; 4] = b"PKFc";

mod access;
mod caps;
mod clock;
mod control;
mod datagram;
mod handshake;
mod pairing;
mod pen;

/// quinn endpoint constructors: host identity ([`endpoint::server_with_identity`]),
/// client pin / TOFU ([`endpoint::client_pinned`]).
pub mod endpoint;

pub mod io;

/// Per-transfer clipboard fetch streams (`PKFs` + kind, then request/response).
/// Transport only; wire codecs in [`control`], state per side.
pub mod clipstream;

/// SPAKE2 over Ed25519 for pairing. Both certificate fingerprints are the SPAKE2
/// identities, so a MITM that presents different certs on each leg cannot share a key.
pub mod pake;

pub use access::*;
pub use caps::*;
pub use clock::*;
pub use control::*;
pub use datagram::*;
pub use handshake::*;
pub use pairing::*;
pub use pen::*;

// Close codes + [`RejectReason`] live in `crate::reject` (ungated: the error enum
// names them even without `quic`) and re-export here next to QUIT/APP_EXITED.
pub use crate::reject::*;

#[cfg(test)]
pub(crate) mod test_util;
