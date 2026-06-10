//! Pluggable packet I/O. The hot path calls [`Transport::send`] / [`Transport::recv`]
//! directly — no async runtime is involved.

mod loopback;
mod udp;

pub use loopback::{loopback_pair, LoopbackTransport};
pub use udp::UdpTransport;

/// A datagram transport. `recv` is non-blocking: it returns `Ok(None)` when no packet
/// is currently available, so the caller (decode/present thread) never blocks here.
pub trait Transport: Send + Sync {
    fn send(&self, packet: &[u8]) -> std::io::Result<()>;
    fn recv(&self) -> std::io::Result<Option<Vec<u8>>>;
}
