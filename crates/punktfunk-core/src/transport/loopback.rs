//! In-process transport for unit tests and the C ABI harness. Two cross-wired
//! [`LoopbackTransport`]s form a host↔client link, with optional deterministic loss so
//! tests can exercise FEC recovery without a real network.

use super::Transport;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct Channel {
    queue: Mutex<VecDeque<Vec<u8>>>,
    /// Drop one of every `drop_period` packets (0 = lossless).
    drop_period: u32,
    sent: AtomicU64,
    dropped: AtomicU64,
}

impl Channel {
    fn new(drop_period: u32) -> Arc<Channel> {
        Arc::new(Channel {
            queue: Mutex::new(VecDeque::new()),
            drop_period,
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        })
    }
}

/// Cross-wired pair half, created by [`loopback_pair`].
pub struct LoopbackTransport {
    tx: Arc<Channel>,
    rx: Arc<Channel>,
}

impl LoopbackTransport {
    pub fn dropped(&self) -> u64 {
        self.tx.dropped.load(Ordering::Relaxed)
    }
}

/// Create a connected `(host, client)` pair. `host_drop_period` injects loss on the
/// host→client (video) path; `client_drop_period` on the reverse (input) path.
pub fn loopback_pair(
    host_drop_period: u32,
    client_drop_period: u32,
) -> (LoopbackTransport, LoopbackTransport) {
    let h2c = Channel::new(host_drop_period);
    let c2h = Channel::new(client_drop_period);
    let host = LoopbackTransport {
        tx: h2c.clone(),
        rx: c2h.clone(),
    };
    let client = LoopbackTransport { tx: c2h, rx: h2c };
    (host, client)
}

impl Transport for LoopbackTransport {
    fn send(&self, packet: &[u8]) -> std::io::Result<bool> {
        let n = self.tx.sent.fetch_add(1, Ordering::Relaxed);
        if self.tx.drop_period != 0 && (n % self.tx.drop_period as u64) == 0 {
            // Network loss: the packet left the sender, then vanished. Report `Ok(true)`
            // so recv/FEC handle it. `Ok(false)` is reserved for WouldBlock send-buffer overflow.
            self.tx.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        }
        self.tx.queue.lock().unwrap().push_back(packet.to_vec());
        Ok(true)
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
        Ok(self.rx.queue.lock().unwrap().pop_front())
    }
}
