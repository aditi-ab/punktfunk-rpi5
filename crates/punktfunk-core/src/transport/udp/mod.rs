//! Real UDP datagram transport — native sockets, no async runtime.
//!
//! Send is batched via `sendmmsg` ([`Transport::send_batch`], ≤64/syscall) and recv via `recvmmsg`
//! ([`Transport::recv_batch`], ≤128/syscall into a reused ring) on Linux AND Android (which is
//! `target_os = "android"`, not `"linux"` — it needs its own bionic binding, see `android_mmsg`)
//! — the 1 Gbps+ syscall lever (~125k → a few-k syscalls/sec at line rate). The host additionally
//! paces each frame's send across the frame interval (see `native.rs::paced_submit`) so a real
//! NIC doesn't drop a line-rate burst. All three layer on this same [`Transport`] seam (scalar
//! fallbacks for loopback and the remaining targets).

use super::Transport;
use crate::packet::MAX_DATAGRAM_BYTES;
use std::net::UdpSocket;

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
mod apple;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::send_uso_all;

/// Receive buffer size. `Config::validate` bounds `shard_payload` so a well-formed
/// datagram (header + shard + crypto overhead) always fits in [`MAX_DATAGRAM_BYTES`];
/// the `+ 1` byte lets us detect an oversized datagram (a full read) instead of
/// silently truncating it.
const RECV_BUF: usize = MAX_DATAGRAM_BYTES + 1;

/// True for transient socket conditions that must be a lossy drop / "no data this poll" — NOT a
/// stream teardown. Two cases:
/// - `WouldBlock`: the kernel send/recv buffer is momentarily full (a frame burst saturated the tx
///   queue — the dominant condition at 1 Gbps+). Drop the packet; FEC + the next frame recover.
/// - `ConnectionRefused` / `ConnectionReset`: a *connected* UDP socket received an asynchronous ICMP
///   port-unreachable / reset for an *earlier* datagram. With data-plane hole-punching the path
///   blips — the peer's data socket briefly gone, a NAT rebind, or a stale ICMP from punch setup —
///   so erroring out here kills a stream that the very next packet would resume. If the peer is
///   genuinely gone, the QUIC control plane times out and ends the session cleanly instead. (This is
///   the classic connected-UDP "ICMP errors are advisory" rule, doubly true with hole-punching.)
/// - `ENOBUFS`: a WiFi/wlan driver (e.g. `ath11k` on the Steam Deck) returns this — NOT `EAGAIN`/
///   `WouldBlock` — when its tx queue is momentarily full. Rust maps `ENOBUFS` to
///   `ErrorKind::Uncategorized`, so the `WouldBlock` arm misses it; without this a transient
///   tx-queue burst tears the whole stream down (observed live: the host streamed flawlessly on
///   loopback / under a debugger — anything slow enough not to fill the small wlan0 buffer — but
///   died at full rate over WiFi). Same lossy-drop contract as `WouldBlock`; FEC + the next frame
///   recover. Asynchronous network-path blips (`ENETUNREACH`/`EHOSTUNREACH`/`ENETDOWN`/`EHOSTDOWN`)
///   are droppable for the same reason a stale ICMP is.
/// - Windows `WSAENOBUFS` (10055): the exact analogue of unix `ENOBUFS` — a high-bitrate keyframe
///   burst (one `WSASendMsg` USO super-buffer is up to ~512 segments ≈ 700 KB) momentarily exhausts
///   the socket send buffer / AFD non-paged pool, and Winsock reports `WSAENOBUFS`, which Rust maps
///   to `ErrorKind::Uncategorized` (so the `WouldBlock` arm misses it, exactly like unix `ENOBUFS`).
///   Without treating it as transient a Windows host tears the whole session down under load
///   (observed live: `native::stream` "send failed — stopping stream" on a paced video burst). Same
///   lossy-drop contract; FEC + the next frame recover. The `WSAENET*`/`WSAEHOST*` family is the
///   Windows counterpart of the droppable unix network-path blips above.
fn is_transient_io(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, ConnectionReset, WouldBlock};
    if matches!(e.kind(), WouldBlock | ConnectionRefused | ConnectionReset) {
        return true;
    }
    // `ENOBUFS` & friends have no stable `ErrorKind`, so match the raw errno.
    #[cfg(unix)]
    {
        matches!(
            e.raw_os_error(),
            Some(libc::ENOBUFS)
                | Some(libc::ENETUNREACH)
                | Some(libc::EHOSTUNREACH)
                | Some(libc::ENETDOWN)
                | Some(libc::EHOSTDOWN)
        )
    }
    // Windows Winsock codes (WSAE*), raw like the sibling `uso_unsupported`. WSAEWOULDBLOCK (10035)
    // already maps to `ErrorKind::WouldBlock` above, so it isn't repeated here.
    #[cfg(windows)]
    {
        matches!(
            e.raw_os_error(),
            Some(10055)   // WSAENOBUFS    — tx queue / send buffer full (the dominant high-bitrate drop)
                | Some(10051) // WSAENETUNREACH
                | Some(10065) // WSAEHOSTUNREACH
                | Some(10050) // WSAENETDOWN
                | Some(10064) // WSAEHOSTDOWN
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Data-plane NAT/firewall hole-punch marker. The video data plane is a raw UDP socket distinct
/// from the QUIC control connection; on a flat LAN the host can send straight to the client, but
/// across a NAT or a stateful inter-VLAN firewall the unsolicited host→client video is rejected
/// (ICMP port-unreachable). So the client sends these tiny datagrams FROM its data socket TO the
/// host's data port: that opens the firewall/NAT return path and lets the host learn the client's
/// *observed* source (the NAT-translated address, not the client's reported private one). It's the
/// only thing a client ever sends on the data plane (video is host→client), so the host treats any
/// punch-magic datagram purely as a source-address probe and never as stream data.
pub const PUNCH_MAGIC: &[u8] = b"PFpunch1";

/// Spawn the client-side data-plane hole-punch keepalive. `sock` is a clone of the data socket
/// (already `connect`ed to the host's data port — see [`UdpTransport::try_clone_socket`]). Bursts
/// fast at first to open the NAT/firewall path before the host's punch-wait expires, then steady
/// keepalive so a stateful firewall's idle timeout can't close the path during a static, low-bitrate
/// scene. Stops when `stop` is set (session teardown) or the socket closes. No-op cost on a flat LAN.
pub fn spawn_data_punch(sock: UdpSocket, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    std::thread::Builder::new()
        .name("punktfunk-data-punch".into())
        .spawn(move || {
            let mut i = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match sock.send(PUNCH_MAGIC) {
                    Ok(_) => {}
                    // Same contract as `Transport::send`: a momentarily full tx queue, a stale
                    // ICMP or a network-path blip is a lossy drop, not a reason to stop holding
                    // the NAT/firewall path open. Breaking here is silent and permanent — the
                    // path recovers, video keeps flowing, and the stream dies later when the
                    // idle timer expires the mapping during a static scene.
                    Err(e) if is_transient_io(&e) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "data-plane punch send failed — stopping keepalive");
                        break;
                    }
                }
                let delay_ms = if i < 15 { 200 } else { 2000 };
                i = i.saturating_add(1);
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
        })
        .ok();
}

pub struct UdpTransport {
    /// qWAVE flow guard (Windows, opt-in DSCP): declared before `socket` so drop order removes
    /// the flow membership before the socket closes. Always `None` off-Windows.
    _qos_flow: Option<super::qos::QosFlow>,
    socket: UdpSocket,
}

impl UdpTransport {
    /// Bind `local` and `connect` to `peer`, so `send`/`recv` need no address and the
    /// kernel filters to this peer. Non-blocking, matching the [`Transport`] contract.
    pub fn connect(local: &str, peer: &str) -> std::io::Result<Self> {
        Self::from_socket(UdpSocket::bind(local)?, peer)
    }

    /// Adopt an already-bound socket for the data plane: `connect` it to `peer`, tune buffers +
    /// QoS, go non-blocking. Lets the host bind the data port up front (e.g. a fixed `--data-port`)
    /// and keep the *same* socket from handshake through streaming — no drop-then-rebind window in
    /// which a concurrent session could steal a fixed port.
    pub fn from_socket(socket: UdpSocket, peer: &str) -> std::io::Result<Self> {
        socket.connect(peer)?;
        super::qos::grow_socket_buffers(&socket);
        // The native data plane is video-dominant — tag it as the video class (opt-in via
        // PUNKTFUNK_DSCP). Each end marks its own egress; the socket is connected by now, as
        // the Windows qWAVE flow requires.
        let qos_flow = super::qos::set_media_qos(&socket, super::qos::MediaClass::Video);
        socket.set_nonblocking(true)?;
        Ok(UdpTransport {
            _qos_flow: qos_flow,
            socket,
        })
    }

    /// Host side of the data plane for clients that may sit behind NAT / a stateful inter-VLAN
    /// firewall. Bind `local`, then block up to `punch_timeout` for the client's first
    /// [`PUNCH_MAGIC`] datagram and `connect` to its *observed* source — so video flows back
    /// through the path the client just opened, to the address+port the host actually sees (the
    /// NAT-translated one, which can differ from the client-reported `fallback_peer`). If no punch
    /// arrives (a client that doesn't hole-punch), fall back to `fallback_peer` — the same flat-LAN
    /// behaviour as [`connect`](Self::connect). Returns `(transport, punched)`.
    ///
    /// `expect_ip` is the *authenticated* peer address (the QUIC connection's remote IP) — see
    /// [`from_socket_punch`](Self::from_socket_punch) for why only punches from it are honoured.
    pub fn connect_via_punch(
        local: &str,
        fallback_peer: &str,
        expect_ip: std::net::IpAddr,
        punch_timeout: std::time::Duration,
    ) -> std::io::Result<(Self, bool)> {
        Self::from_socket_punch(
            UdpSocket::bind(local)?,
            fallback_peer,
            expect_ip,
            punch_timeout,
        )
    }

    /// [`connect_via_punch`](Self::connect_via_punch) on an already-bound socket — see
    /// [`from_socket`](Self::from_socket) for why the host binds the data port up front.
    ///
    /// `expect_ip` binds the data plane to the peer the control plane already authenticated.
    /// [`PUNCH_MAGIC`] is a fixed public constant carrying no key, nonce or session id, so without
    /// this check *any* source that lands an 8-byte datagram on the (ephemeral, sprayable) data
    /// port during the punch wait becomes the video destination — the legitimate client is then
    /// filtered out by the `connect` below and receives nothing, while QUIC stays healthy so no
    /// reconnect is triggered. Only the *port* is in question here (that is what a NAT remaps, and
    /// what the punch exists to discover); the IP is known, because the client binds `0.0.0.0:0`
    /// and dials the same host IP as its QUIC connection, so the kernel picks the same source IP
    /// for both planes and any NAT on the path presents one source IP for both.
    pub fn from_socket_punch(
        socket: UdpSocket,
        fallback_peer: &str,
        expect_ip: std::net::IpAddr,
        punch_timeout: std::time::Duration,
    ) -> std::io::Result<(Self, bool)> {
        let deadline = std::time::Instant::now() + punch_timeout;
        let mut buf = [0u8; 64];
        let mut observed: Option<std::net::SocketAddr> = None;
        loop {
            // Budget the read from what's LEFT, not the full window: off-peer datagrams are
            // discarded below, and a full-window timeout per read would let a stray flood stretch
            // the punch wait far past `punch_timeout`.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            socket.set_read_timeout(Some(remaining))?;
            match socket.recv_from(&mut buf) {
                Ok((n, src))
                    if src.ip() == expect_ip
                        && n >= PUNCH_MAGIC.len()
                        && &buf[..PUNCH_MAGIC.len()] == PUNCH_MAGIC =>
                {
                    observed = Some(src);
                    break;
                }
                // Stray, or a well-formed punch from someone who isn't the authenticated peer —
                // keep waiting for a real one.
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break
                }
                Err(e) => return Err(e),
            }
        }
        let punched = observed.is_some();
        let target = observed.map(|s| s.to_string());
        socket.connect(target.as_deref().unwrap_or(fallback_peer))?;
        socket.set_read_timeout(None)?;
        super::qos::grow_socket_buffers(&socket);
        let qos_flow = super::qos::set_media_qos(&socket, super::qos::MediaClass::Video);
        socket.set_nonblocking(true)?;
        Ok((
            UdpTransport {
                _qos_flow: qos_flow,
                socket,
            },
            punched,
        ))
    }

    /// A second handle to the data socket, for sending hole-punch keepalives ([`PUNCH_MAGIC`])
    /// while the [`Session`](crate::Session) owns the transport. The socket is already `connect`ed
    /// to the host's data port, so `clone.send(PUNCH_MAGIC)` reaches it with no address.
    pub fn try_clone_socket(&self) -> std::io::Result<UdpSocket> {
        self.socket.try_clone()
    }

    /// The bound local address (e.g. to learn the OS-assigned ephemeral port).
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.socket.local_addr()
    }
}

impl Transport for UdpTransport {
    fn send(&self, packet: &[u8]) -> std::io::Result<bool> {
        match self.socket.send(packet) {
            Ok(_) => Ok(true),
            // The kernel UDP send buffer is momentarily full (a frame burst saturated the
            // tx queue — common right after attaching to an already-running source that
            // emits at full rate, and the dominant failure mode at 1 Gbps+). Drop this packet
            // rather than fail the whole stream: the data plane is lossy + FEC-protected and the
            // next frame/RFI keyframe recovers, whereas blocking would queue stale frames and add
            // latency, and erroring tears the session down. `Ok(false)` surfaces the drop so the
            // session counts it (`packets_send_dropped`) instead of it being invisible. Mirrors
            // the `recv` WouldBlock handling above.
            Err(e) if is_transient_io(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Batched send via `sendmmsg` (up to 64 datagrams per syscall) — the connected socket needs
    /// no per-message address. The socket is non-blocking, so a full send buffer surfaces as a
    /// short count (or `EAGAIN` with nothing sent); we stop and report what went out rather than
    /// block or retry — the data plane is lossy + FEC-protected, and blocking would queue stale
    /// frames + add latency. Ports the proven GameStream `sendmmsg_all`. Other targets fall back
    /// to the trait's scalar `send` loop (no `sendmmsg`).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn send_batch(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        linux::send_batch(self, packets)
    }

    /// UDP GSO send (see [`Transport::send_gso`]). Coalesces the frame's equal-size packets into a
    /// reused scratch buffer and hands the kernel ≤64-segment super-buffers via `sendmsg(UDP_SEGMENT)`
    /// — one GSO skb per chunk instead of one per packet, the multi-Gbps lever. Opt-in
    /// (`PUNKTFUNK_GSO`); falls back to `send_batch` when off, when packets aren't uniform-size, or on
    /// any GSO error (which also latches it off for the process). Same lossy short-count contract.
    #[cfg(target_os = "linux")]
    fn send_gso(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        linux::send_gso(self, packets)
    }

    /// UDP USO send (see [`Transport::send_gso`]) — Windows. Coalesces the frame's equal-size packets
    /// and hands Winsock ≤512-segment super-buffers via `WSASendMsg(UDP_SEND_MSG_SIZE)` — one syscall
    /// per chunk instead of one `send` per packet, the 1 Gbps+ lever (Windows analogue of Linux GSO).
    /// On by default (kill: `PUNKTFUNK_GSO=0`); falls back to the scalar `send_batch` when off, when
    /// packets aren't uniform-size, or on a USO-unsupported error (which latches it off for the
    /// process). Same lossy short-count contract.
    #[cfg(target_os = "windows")]
    fn send_gso(&self, packets: &[&[u8]]) -> std::io::Result<usize> {
        windows::send_gso(self, packets)
    }

    fn recv(&self) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; RECV_BUF];
        match self.socket.recv(&mut buf) {
            // A read that fills the whole buffer means the datagram was larger than any
            // valid packet — drop it rather than hand a truncated, corrupt packet up.
            Ok(n) if n >= RECV_BUF => Ok(None),
            Ok(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            Err(e) if is_transient_io(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Batched receive via `recvmmsg` — drains up to `out.len()` datagrams in one syscall into the
    /// caller's reused buffers (no per-packet allocation). `MSG_DONTWAIT` keeps it non-blocking
    /// (the socket already is); `EAGAIN` → `0`. A datagram larger than a buffer is truncated and
    /// `lens[i]` reaches the buffer size — the reassembler then rejects it as malformed, matching
    /// `recv`'s oversized-drop. Android uses the local bionic binding (see `android_mmsg`).
    /// Apple/BSD use the `recv`-loop override below; other non-unix the trait's scalar default.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn recv_batch(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        linux::recv_batch(self, out, lens)
    }

    /// Batched receive for Apple/BSD targets, which have no `recvmmsg(2)`. Drains up to `out.len()`
    /// datagrams per call with `libc::recv(MSG_DONTWAIT)` straight into the caller's reused `out[i]`
    /// buffers — eliminating the per-packet 2 KB `vec!` allocation (and its zeroing + a copy) that
    /// the scalar `recv` + trait-default `recv_batch` incur. THIS is the macOS-client throughput
    /// fix: at line rate the alloc/free churn — not the syscall — was the single-core wall (Moonlight
    /// batches; our client per-packet-allocated). It is still one syscall per datagram (a future
    /// `recvmsg_x` batch would cut that too); `EAGAIN` ends the drain. Oversized datagrams set
    /// `lens[i] == buf.len()` and the caller (`poll_frame`) drops them — same contract as `recvmmsg`.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    fn recv_batch(&self, out: &mut [Vec<u8>], lens: &mut [usize]) -> std::io::Result<usize> {
        apple::recv_batch(self, out, lens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    /// A connected UDP socket's stale ICMP (ECONNREFUSED/RESET) and a full buffer (EAGAIN) must all
    /// be classified transient — a lossy drop, never a stream teardown. A real error must not be.
    #[test]
    fn transient_io_covers_connected_udp_blips() {
        use std::io::{Error, ErrorKind};
        for k in [
            ErrorKind::WouldBlock,
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
        ] {
            assert!(
                is_transient_io(&Error::from(k)),
                "{k:?} should be transient"
            );
        }
        for k in [ErrorKind::PermissionDenied, ErrorKind::AddrInUse] {
            assert!(!is_transient_io(&Error::from(k)), "{k:?} must stay fatal");
        }
    }

    /// The raw-errno tx-queue-full / network-blip codes have no stable `ErrorKind` (they surface as
    /// `Uncategorized`), so they only get caught by the platform `raw_os_error()` arms. A burst that
    /// momentarily exhausts the send buffer must stay a lossy drop, never a teardown — this is the
    /// regression guard for the Windows `WSAENOBUFS` (10055) session crash and the unix `ENOBUFS`
    /// wlan-driver case. Gated per platform because a code is only classified on its own OS.
    #[test]
    fn transient_io_covers_raw_tx_queue_and_path_codes() {
        use std::io::Error;

        #[cfg(unix)]
        {
            for code in [
                libc::ENOBUFS,
                libc::ENETUNREACH,
                libc::EHOSTUNREACH,
                libc::ENETDOWN,
                libc::EHOSTDOWN,
            ] {
                assert!(
                    is_transient_io(&Error::from_raw_os_error(code)),
                    "unix errno {code} should be transient"
                );
            }
            // A genuine failure with no stable ErrorKind must still tear down.
            assert!(
                !is_transient_io(&Error::from_raw_os_error(libc::EACCES)),
                "EACCES must stay fatal"
            );
        }

        #[cfg(windows)]
        {
            // WSAENOBUFS / WSAENETUNREACH / WSAEHOSTUNREACH / WSAENETDOWN / WSAEHOSTDOWN.
            for code in [10055, 10051, 10065, 10050, 10064] {
                assert!(
                    is_transient_io(&Error::from_raw_os_error(code)),
                    "WSA code {code} should be transient"
                );
            }
            // WSAEACCES (10013) — a real failure that must stay fatal.
            assert!(
                !is_transient_io(&Error::from_raw_os_error(10013)),
                "WSAEACCES must stay fatal"
            );
        }
    }

    /// `send_batch` delivers a whole frame's worth of packets over real loopback UDP — exercising
    /// the `sendmmsg` path on Linux (the scalar-loop default elsewhere). 100 × 200 B = 20 KB fits
    /// the socket buffer, so loopback is lossless and every packet must arrive intact + in order.
    #[test]
    fn send_batch_delivers_over_loopback() {
        let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        let rx_addr = rx.local_addr().unwrap().to_string();
        let tx = UdpTransport::connect("127.0.0.1:0", &rx_addr).unwrap();

        const N: u32 = 100;
        let payloads: Vec<Vec<u8>> = (0..N)
            .map(|i| {
                let mut v = vec![0u8; 200];
                v[0..4].copy_from_slice(&i.to_le_bytes());
                v
            })
            .collect();
        let refs: Vec<&[u8]> = payloads.iter().map(|p| p.as_slice()).collect();
        let sent = tx.send_batch(&refs).unwrap();
        assert_eq!(
            sent, N as usize,
            "send_batch should hand all packets to the kernel"
        );

        let mut seen = std::collections::HashSet::new();
        let mut buf = [0u8; 2048];
        while seen.len() < N as usize {
            match rx.recv(&mut buf) {
                Ok(n) => {
                    assert_eq!(
                        n, 200,
                        "datagram boundaries preserved (one packet per recv)"
                    );
                    seen.insert(u32::from_le_bytes(buf[0..4].try_into().unwrap()));
                }
                Err(_) => break, // read timeout — stop and let the assert report the shortfall
            }
        }
        assert_eq!(
            seen.len(),
            N as usize,
            "every batched packet should arrive over loopback"
        );
    }

    /// `recv_batch` drains many datagrams per call over real loopback UDP — exercising `recvmmsg`
    /// on Linux (the scalar `recv` default elsewhere). Send 50 distinct packets, then drain in
    /// batches and assert every one arrives intact with the right length.
    #[test]
    fn recv_batch_drains_over_loopback() {
        // Receiver is the UdpTransport (the thing under test); sender is a raw socket bound to a
        // known addr so the connected receiver accepts its datagrams.
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let tx_addr = tx.local_addr().unwrap().to_string();
        let rx = UdpTransport::connect("127.0.0.1:0", &tx_addr).unwrap();
        let rx_addr = rx.local_addr().unwrap();

        const N: u32 = 50;
        for i in 0..N {
            let mut p = vec![0u8; 300];
            p[0..4].copy_from_slice(&i.to_le_bytes());
            tx.send_to(&p, rx_addr).unwrap();
        }

        let mut bufs: Vec<Vec<u8>> = (0..16).map(|_| vec![0u8; RECV_BUF]).collect();
        let mut lens = vec![0usize; 16];
        let mut seen = std::collections::HashSet::new();
        // A few drains absorb scheduling jitter; stop once all N are in or we go dry.
        for _ in 0..50 {
            let n = rx.recv_batch(&mut bufs, &mut lens).unwrap();
            if n == 0 {
                if seen.len() == N as usize {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            for i in 0..n {
                assert_eq!(lens[i], 300, "recvmmsg reports the datagram length");
                seen.insert(u32::from_le_bytes(bufs[i][0..4].try_into().unwrap()));
            }
        }
        assert_eq!(
            seen.len(),
            N as usize,
            "every datagram should be drained via recv_batch"
        );
    }

    /// The punch discovers the peer's NAT-remapped *port*, so a punch from the authenticated IP on
    /// a port that differs from the client-reported one must still be adopted — that is the whole
    /// reason hole-punching exists, and the source-IP check must not break it.
    #[test]
    fn punch_adopts_remapped_port_from_the_authenticated_peer() {
        // Stands in for the client's post-NAT data socket: same IP as the "QUIC peer", new port.
        let puncher = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        puncher
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        // The client-*reported* address, which the NAT remapped — video must NOT go here.
        let reported = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        reported
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();

        let host_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_addr = host_sock.local_addr().unwrap();
        puncher.send_to(PUNCH_MAGIC, host_addr).unwrap();

        let (transport, punched) = UdpTransport::from_socket_punch(
            host_sock,
            &reported.local_addr().unwrap().to_string(),
            std::net::IpAddr::from([127, 0, 0, 1]),
            std::time::Duration::from_millis(500),
        )
        .unwrap();
        assert!(punched, "a punch from the authenticated IP must be adopted");

        transport.send(b"video").unwrap();
        let mut buf = [0u8; 64];
        let n = puncher
            .recv(&mut buf)
            .expect("video must follow the punched (NAT-remapped) port");
        assert_eq!(&buf[..n], b"video");
        assert!(
            reported.recv(&mut buf).is_err(),
            "video must not go to the stale reported port"
        );
    }

    /// A punch from any source other than the QUIC-authenticated peer must be ignored: `PUNCH_MAGIC`
    /// is a fixed public constant with no key or session id, so honouring an off-peer punch lets
    /// anyone who lands an 8-byte datagram on the ephemeral data port steal (or redirect) the video
    /// plane while the control plane stays healthy. Falling back to the reported address is correct.
    #[test]
    fn punch_from_an_unauthenticated_source_is_ignored() {
        let attacker = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        attacker
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        let legit = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        legit
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();

        let host_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_addr = host_sock.local_addr().unwrap();
        attacker.send_to(PUNCH_MAGIC, host_addr).unwrap();

        // The authenticated peer is TEST-NET-1, so nothing arriving over loopback is the peer.
        let (transport, punched) = UdpTransport::from_socket_punch(
            host_sock,
            &legit.local_addr().unwrap().to_string(),
            std::net::IpAddr::from([192, 0, 2, 1]),
            std::time::Duration::from_millis(300),
        )
        .unwrap();
        assert!(
            !punched,
            "an off-peer punch must not be adopted as the video destination"
        );

        transport.send(b"video").unwrap();
        let mut buf = [0u8; 64];
        assert!(
            attacker.recv(&mut buf).is_err(),
            "video must never be redirected to the punch source"
        );
        let n = legit
            .recv(&mut buf)
            .expect("video falls back to the reported peer address");
        assert_eq!(&buf[..n], b"video");
    }
}
