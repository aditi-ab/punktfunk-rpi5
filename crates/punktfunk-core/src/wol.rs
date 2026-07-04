//! Wake-on-LAN: magic-packet builder + broadcast sender.
//!
//! Runtime-free by design — a magic packet is one fire-and-forget UDP datagram, so this needs
//! neither the `quic` feature nor an async runtime and links into every client (including the
//! QUIC-less builds). The Rust clients (linux/windows/android) call these `pub fn`s directly;
//! Swift/iOS reach them through the `punktfunk_wake_on_lan` C-ABI wrapper in [`crate::abi`].
//!
//! Reliability (this is the whole point — a sleeping host has no ARP entry, so a plain unicast
//! can't wake it, and `255.255.255.255` alone leaves only via the default route). For each
//! known host MAC we send the 102-byte packet to:
//!   * every non-loopback IPv4 interface's **subnet-directed broadcast** (routes to that NIC's
//!     segment — this is what covers multi-homed clients on VPN/docker/multiple LANs), and
//!   * the **limited broadcast** `255.255.255.255`, and
//!   * optionally a **unicast** to the host's last-known IP (covers the brief window where the
//!     host is reachable but hasn't re-advertised, and NICs that wake on a directed unicast),
//! on the two conventional WoL ports (9 and 7), repeated a few times to survive UDP loss.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

/// A MAC address (EUI-48), the 6 bytes a magic packet targets.
pub type Mac = [u8; 6];

/// Conventional Wake-on-LAN UDP ports. 9 (discard) is by far the most common; 7 (echo) is a
/// historical alternative some NICs also listen on. Sending to both is free insurance.
const WOL_PORTS: [u16; 2] = [9, 7];

/// Times each packet is re-sent per call. UDP is lossy and this is fire-and-forget; a small
/// burst costs microseconds and materially improves the odds a waking NIC catches one. The
/// caller's connect-retry loop provides the longer-spaced re-attempts.
const BURST: usize = 3;

/// Parse a MAC string — `aa:bb:cc:dd:ee:ff` or `aa-bb-...`, case-insensitive — into 6 bytes.
/// Returns `None` for anything that isn't exactly six hex octets. Shared by the Rust clients
/// (linux/windows) so MAC parsing lives in one place; the Swift/Apple client parses its own.
pub fn parse_mac(s: &str) -> Option<Mac> {
    let mut m = [0u8; 6];
    let mut n = 0;
    for part in s.split(|c| c == ':' || c == '-') {
        if n == 6 {
            return None; // too many octets
        }
        m[n] = u8::from_str_radix(part.trim(), 16).ok()?;
        n += 1;
    }
    (n == 6).then_some(m)
}

/// The 102-byte magic packet for `mac`: 6×`0xFF` followed by the MAC repeated 16 times.
pub fn build_magic_packet(mac: Mac) -> [u8; 102] {
    let mut pkt = [0xFFu8; 102];
    for i in 0..16 {
        let off = 6 + i * 6;
        pkt[off..off + 6].copy_from_slice(&mac);
    }
    pkt
}

/// Broadcast a wake for every MAC in `macs`. `last_known_ip`, when set, is additionally
/// targeted by unicast.
///
/// Returns `Ok` if at least one datagram was sent, so a single unreachable target (e.g. a
/// directed broadcast with no route) doesn't fail the whole wake. Errors only if no socket
/// could be opened or nothing could be sent at all.
pub fn send_magic_packet(macs: &[Mac], last_known_ip: Option<Ipv4Addr>) -> io::Result<()> {
    if macs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no MAC addresses",
        ));
    }

    // Build the target IP set: each interface's directed broadcast, the limited broadcast, and
    // the optional last-known unicast. Dedup so a single-NIC client doesn't send twice.
    let mut targets = broadcast_addrs();
    targets.push(Ipv4Addr::BROADCAST); // 255.255.255.255
    if let Some(ip) = last_known_ip {
        targets.push(ip);
    }
    targets.sort_unstable();
    targets.dedup();

    // One broadcast-enabled socket bound to all interfaces. Directed broadcasts route to the
    // matching NIC via the routing table; the limited broadcast leaves via the default route.
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_broadcast(true)?;

    let mut sent_any = false;
    for _ in 0..BURST {
        for mac in macs {
            let pkt = build_magic_packet(*mac);
            for ip in &targets {
                for port in WOL_PORTS {
                    let dst = SocketAddr::V4(SocketAddrV4::new(*ip, port));
                    if sock.send_to(&pkt, dst).is_ok() {
                        sent_any = true;
                    }
                }
            }
        }
    }

    if sent_any {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "no magic packet could be sent",
        ))
    }
}

/// Subnet-directed broadcast address of every non-loopback IPv4 interface (`ip | !netmask`,
/// or the OS-provided broadcast when present). Best-effort: interface enumeration failing
/// (permissions, exotic platform) yields an empty list, and the limited broadcast still fires.
fn broadcast_addrs() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(i) => i,
        Err(_) => return out,
    };
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            let bcast = v4
                .broadcast
                .unwrap_or_else(|| Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask)));
            // Skip a degenerate 0.0.0.0 (unconfigured) and the all-ones limited broadcast we
            // already add unconditionally.
            if !bcast.is_unspecified() && bcast != Ipv4Addr::BROADCAST {
                out.push(bcast);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_layout() {
        let mac: Mac = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let pkt = build_magic_packet(mac);
        assert_eq!(pkt.len(), 102);
        // 6-byte 0xFF sync stream.
        assert_eq!(&pkt[0..6], &[0xFF; 6]);
        // MAC repeated exactly 16 times.
        for i in 0..16 {
            let off = 6 + i * 6;
            assert_eq!(&pkt[off..off + 6], &mac, "repetition {i} mismatch");
        }
    }

    #[test]
    fn empty_macs_is_error() {
        assert!(send_magic_packet(&[], None).is_err());
    }

    #[test]
    fn parse_mac_forms() {
        assert_eq!(
            parse_mac("aa:bb:cc:dd:ee:ff"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert_eq!(
            parse_mac("AA-BB-CC-DD-EE-FF"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert_eq!(parse_mac("01:02:03:04:05:06"), Some([1, 2, 3, 4, 5, 6]));
        assert_eq!(parse_mac("aa:bb:cc:dd:ee"), None); // too few
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:ff:00"), None); // too many
        assert_eq!(parse_mac("zz:bb:cc:dd:ee:ff"), None); // non-hex
        assert_eq!(parse_mac(""), None);
    }

    #[test]
    fn send_does_not_panic_with_a_mac() {
        // Best-effort: binds a real socket and broadcasts on the loopback host. Must not panic
        // and, on any machine with a usable network stack, should report success.
        let _ = send_magic_packet(&[[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]], None);
    }

    #[test]
    fn broadcast_addrs_never_contains_limited_or_unspecified() {
        for b in broadcast_addrs() {
            assert_ne!(b, Ipv4Addr::BROADCAST);
            assert!(!b.is_unspecified());
        }
    }
}
