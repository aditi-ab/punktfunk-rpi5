//! # punktfunk-core
//!
//! The shared protocol / transport / FEC core for the punktfunk low-latency streaming
//! stack. It is compiled exactly once and linked by every host and client — directly
//! as a Rust `lib`, or across the [C ABI](crate::abi) by Swift / Kotlin / C clients.
//!
//! Everything platform-specific (capture, encode, decode, present, input injection)
//! lives *outside* this crate. What lives *here*:
//!
//! - [`fec`] — erasure coding. GF(2⁸) for GameStream/Moonlight compatibility (P1) and
//!   GF(2¹⁶) Leopard-RS (P2) which removes the ~1 Gbps per-frame shard-count ceiling.
//! - [`packet`] — `#[repr(C)]` zero-copy wire framing: splitting an access unit into
//!   FEC blocks of MTU-sized shards and reassembling them on the far side.
//! - [`crypto`] — AES-128-GCM session sealing, matching GameStream in P1.
//! - [`session`] — the host (submit frame → FEC → packetize → seal → send) and client
//!   (recv → open → reorder → FEC recover → reassemble) state machines.
//! - [`transport`] — pluggable packet I/O (in-process loopback for tests; UDP for real).
//! - [`abi`] — the `extern "C"` surface and `cbindgen`-generated `punktfunk_core.h`.
//!
//! ## Threading contract
//!
//! Nothing in the per-frame path touches an async runtime. `tokio`/`quinn` are gated
//! behind the off-by-default `quic` feature and used only for the control plane.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod abi;
#[cfg(feature = "quic")]
mod abr;
pub mod audio;
#[cfg(feature = "quic")]
pub mod client;
pub mod config;
pub mod crypto;
pub mod error;
pub mod fec;
pub mod input;
pub mod packet;
#[cfg(feature = "quic")]
pub mod quic;
pub mod session;
pub mod stats;
pub mod transport;
pub mod wol;

pub use config::{CompositorPref, Config, FecConfig, FecScheme, Mode, ProtocolPhase, Role};
pub use error::{PunktfunkError, PunktfunkStatus, Result};
pub use session::{Frame, Session};
pub use stats::Stats;

/// Bump on any breaking change to the [C ABI](crate::abi). Mirrors
/// `punktfunk_abi_version()` and is checked by clients before use.
///
/// v2: `punktfunk_connect` gained `client_cert_pem`/`client_key_pem` (pairing identities);
/// added `punktfunk_pair` / `punktfunk_generate_identity` / `punktfunk_connection_request_mode`.
/// v3: added `punktfunk_wake_on_lan` (Wake-on-LAN magic packet; the host's wake MAC(s) reach
/// clients out-of-band via the mDNS `mac` TXT record, so no connection is required to wake).
/// v4: added `punktfunk_probe` (bounded, trust-agnostic, mDNS-independent reachability handshake —
/// the display-side companion to dial-first, so saved-host "online" pips reflect real reachability).
pub const ABI_VERSION: u32 = 4;

/// The punktfunk/1 **wire** version — what `Hello`/`Welcome` carry and hosts equality-check.
/// Deliberately its own constant: [`ABI_VERSION`] tracks the embeddable **C surface**
/// (functions a client links), which can grow without changing a single wire byte — v3's
/// `punktfunk_wake_on_lan` is client-local, and riding the C-ABI bump onto the wire locked
/// every new client out of every deployed host ("ABI mismatch: client 3 host 2", observed
/// live). Bump this ONLY when the handshake/planes actually change incompatibly.
pub const WIRE_VERSION: u32 = 2;
