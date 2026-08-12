//! Wake-on-LAN: magic-packet builder + broadcast sender.
//!
//! Runtime-free by design — a magic packet is one fire-and-forget UDP datagram, so this needs
//! neither the `quic` feature nor an async runtime and links into every client (including the
//! QUIC-less builds). The Rust clients (linux/windows/android) call these `pub fn`s directly;
//! Swift/iOS reach them through the `punktfunk_wake_on_lan` C-ABI wrapper in [`crate::abi`].
//!
//! Reliability (this is the whole point — a sleeping host has no ARP entry, so a plain unicast
//! can't wake it, and `255.255.255.255` alone leaves only via the default route). For each
//! known host MAC we send the 102-byte packet:
//!   * **out of every non-loopback IPv4 interface**, from a socket bound to that interface's own
//!     address, to both that NIC's **subnet-directed broadcast** and the **limited broadcast**
//!     `255.255.255.255` — binding the source is what forces the datagram onto that segment
//!     instead of whatever the default route happens to be (a VPN/mesh interface, typically), and
//!   * from an unbound socket to `255.255.255.255` and, when known, a **unicast** to the host's
//!     last-known IP (covers the brief window where the host is reachable but hasn't
//!     re-advertised, and NICs that wake on a directed unicast),
//!
//! on the two conventional WoL ports (9 and 7), repeated a few times to survive UDP loss.
//!
//! **Wi-Fi hosts (WoWLAN) ride the same path**, and the per-interface egress above is what makes
//! them work: a station in WoWLAN sleep stays associated, and the AP buffers broadcast frames for
//! its sleeping stations and flushes them on the next DTIM beacon — so the broadcast does reach
//! the sleeping NIC, but only if the datagram actually leaves via the wireless interface. The
//! host end of it (arming the NIC's magic-packet trigger) is `punktfunk-host`'s `wol` module.

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
    for part in s.split([':', '-']) {
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
    send_magic_packet_on(macs, last_known_ip, &WOL_PORTS)
}

/// [`send_magic_packet`] with the destination ports spelled out. Private because the ports are
/// not a caller's business — it exists so the tests can aim a real send at a port they're allowed
/// to bind (9 and 7 are privileged) and assert the bytes that come off the wire.
fn send_magic_packet_on(
    macs: &[Mac],
    last_known_ip: Option<Ipv4Addr>,
    ports: &[u16],
) -> io::Result<()> {
    if macs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no MAC addresses",
        ));
    }
    let packets: Vec<[u8; 102]> = macs.iter().map(|m| build_magic_packet(*m)).collect();

    // Targets that go out the default route (or wherever the routing table sends them): the
    // limited broadcast as a baseline, plus the optional unicast — destination routing picks the
    // right NIC for a unicast, so it doesn't need per-interface treatment.
    let mut routed: Vec<Ipv4Addr> = vec![Ipv4Addr::BROADCAST];
    if let Some(ip) = last_known_ip {
        routed.push(ip);
    }

    let mut sent_any = false;

    // Per-interface pass. One socket per non-loopback IPv4 address, bound to that address so the
    // datagram leaves on THAT segment: without this, `255.255.255.255` follows the default route
    // only (a VPN/mesh NIC on most of these machines) and never touches the LAN — or the Wi-Fi
    // segment the sleeping WoWLAN station is associated to.
    for (local, bcast) in local_v4_segments() {
        let Ok(sock) = UdpSocket::bind(SocketAddrV4::new(local, 0)) else {
            // Bind failed (address just went away, or the OS refuses it) — fall back to the
            // routed socket below, which still reaches this segment's directed broadcast.
            routed.push(bcast);
            continue;
        };
        if sock.set_broadcast(true).is_err() {
            routed.push(bcast);
            continue;
        }
        sent_any |= blast(&sock, &packets, &[bcast, Ipv4Addr::BROADCAST], ports);
    }

    // Routed pass, and the only pass on a machine whose interfaces can't be enumerated.
    if let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        // A refused SO_BROADCAST doesn't abort the pass: the unicast target still goes out, and
        // the per-interface sockets above may already have carried the broadcast.
        let _ = sock.set_broadcast(true);
        routed.sort_unstable();
        routed.dedup();
        sent_any |= blast(&sock, &packets, &routed, ports);
    } else if !sent_any {
        return Err(io::Error::other("no socket could be opened for the wake"));
    }

    if sent_any {
        Ok(())
    } else {
        Err(io::Error::other("no magic packet could be sent"))
    }
}

/// Send every packet to every target, on every port, [`BURST`] times. Returns whether any
/// single datagram made it out — an unroutable target is expected and never fails the wake.
fn blast(sock: &UdpSocket, packets: &[[u8; 102]], targets: &[Ipv4Addr], ports: &[u16]) -> bool {
    let mut sent_any = false;
    for _ in 0..BURST {
        for pkt in packets {
            for ip in targets {
                // A degenerate 0.0.0.0 (unconfigured NIC) is not a destination.
                if ip.is_unspecified() {
                    continue;
                }
                for port in ports {
                    let dst = SocketAddr::V4(SocketAddrV4::new(*ip, *port));
                    if sock.send_to(pkt, dst).is_ok() {
                        sent_any = true;
                    }
                }
            }
        }
    }
    sent_any
}

/// Every non-loopback IPv4 interface as `(its own address, its subnet-directed broadcast)`. The
/// broadcast is the OS-provided one where present, else `ip | !netmask`. Best-effort: enumeration
/// failing (permissions, exotic platform) yields an empty list and the routed pass still fires.
fn local_v4_segments() -> Vec<(Ipv4Addr, Ipv4Addr)> {
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
            if v4.ip.is_unspecified() {
                continue; // nothing to bind to
            }
            let bcast = v4
                .broadcast
                .unwrap_or_else(|| Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask)));
            out.push((v4.ip, bcast));
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
    fn local_segments_are_bindable_and_have_a_broadcast() {
        for (local, bcast) in local_v4_segments() {
            // The local address is what we bind the per-interface socket to, so it must be a
            // real address — and it must never be the loopback (filtered) or unspecified.
            assert!(!local.is_unspecified());
            assert!(!local.is_loopback());
            assert!(!bcast.is_unspecified());
            // Binding to an address the OS just reported must work; a failure here would mean
            // the per-interface pass silently degrades to the routed one.
            assert!(UdpSocket::bind(SocketAddrV4::new(local, 0)).is_ok());
        }
    }

    #[test]
    fn blast_reports_nothing_sent_for_an_empty_target_list() {
        let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let pkt = [build_magic_packet([1, 2, 3, 4, 5, 6])];
        assert!(!blast(&sock, &pkt, &[], &WOL_PORTS));
        // An unconfigured 0.0.0.0 target is skipped rather than sent to.
        assert!(!blast(&sock, &pkt, &[Ipv4Addr::UNSPECIFIED], &WOL_PORTS));
        // Loopback is a real destination — this one must go out.
        assert!(blast(&sock, &pkt, &[Ipv4Addr::LOCALHOST], &[9999]));
    }

    /// The whole send path, end to end: a real receiver gets a real magic packet with the right
    /// bytes. Aimed at loopback on an unprivileged port (WoL's own 9 and 7 need root to bind),
    /// which exercises the routed pass's unicast leg — the one a WoWLAN host is woken by when
    /// the AP filters broadcast to sleeping stations.
    #[test]
    fn send_delivers_the_magic_packet_to_a_listener() {
        let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind receiver");
        let port = rx.local_addr().expect("local addr").port();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("read timeout");

        let mac: Mac = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        send_magic_packet_on(&[mac], Some(Ipv4Addr::LOCALHOST), &[port]).expect("send");

        let mut buf = [0u8; 256];
        let (n, _from) = rx.recv_from(&mut buf).expect("a magic packet must arrive");
        assert_eq!(n, 102);
        assert_eq!(buf[..102], build_magic_packet(mac));
    }
}
