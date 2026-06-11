//! Pluggable packet I/O. The hot path calls [`Transport::send`] / [`Transport::recv`]
//! directly — no async runtime is involved.

mod loopback;
mod udp;

pub use loopback::{loopback_pair, LoopbackTransport};
pub use udp::UdpTransport;

/// A datagram transport. `recv` is non-blocking: it returns `Ok(None)` when no packet
/// is currently available, so the caller (decode/present thread) never blocks here.
pub trait Transport: Send + Sync {
    /// Send one packet. `Ok(true)` = handed to the kernel; `Ok(false)` = dropped locally because
    /// the send buffer was momentarily full (WouldBlock) — a non-fatal loss the FEC/keyframe path
    /// recovers, surfaced so the caller can count it (`packets_send_dropped`) instead of it being
    /// invisible. `Err` = a real send failure.
    fn send(&self, packet: &[u8]) -> std::io::Result<bool>;
    fn recv(&self) -> std::io::Result<Option<Vec<u8>>>;
}
