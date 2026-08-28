//! Shared discovery-address selection: which A record to dial when an mDNS advert resolves to
//! several.
//!
//! The resolved set is a UNION of answers from every responder on every interface. The host's
//! own advert registers exactly one address (its routed primary — see the host crate's
//! `discovery.rs`), but the host OS's built-in mDNS responder also answers A queries for the
//! same `<host>.local.` label per interface, with that interface's address — so a host running
//! an overlay network (ZeroTier, Tailscale, …) whose multicast reaches this client contributes
//! its overlay address to the set. Field case: a client dialed the host's ZeroTier address
//! while both machines shared a LAN, because the pick was `HashSet::iter().next()` — arbitrary,
//! and re-rolled on every re-announce.
//!
//! [`rank_host_addr`] is the pure policy (testable); [`pick_host_addr`] applies it with this
//! machine's live context.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// Common leading bits of two addresses — the "how on-link is this" proxy the ranking runs on.
/// No netmasks: a longer shared prefix with one of our own addresses is monotonically "more
/// likely on this segment", which is all a RANKING needs.
fn prefix_bits(a: Ipv4Addr, b: Ipv4Addr) -> u32 {
    (u32::from(a) ^ u32::from(b)).leading_zeros()
}

/// The address to dial, chosen deterministically. Score, best wins, in order:
///
/// 1. longest common prefix with ANY of this machine's unicast addresses — an address on one of
///    our own subnets beats one we would have to route. This alone settles the overlay case in
///    both directions: on a shared LAN the host's LAN address out-prefixes its overlay address,
///    and a client that can ONLY reach the host through the overlay has no LAN interface for
///    the host's LAN address to match, so the overlay address wins instead;
/// 2. the address the host itself declared (mDNS TXT `addr`, its routed primary) — settles a
///    multi-NIC host's tie without ever overriding reachability, because a declared address we
///    cannot see on-link already lost rung 1;
/// 3. longest common prefix with OUR routed (default-route) source address — a host that
///    predates the `addr` TXT still resolves the common ties here;
/// 4. the numerically lowest address — pure determinism, so a re-announce cannot flap the pick.
pub fn rank_host_addr(
    candidates: &[Ipv4Addr],
    host_declared: Option<Ipv4Addr>,
    local_ips: &[Ipv4Addr],
    routed_local: Option<Ipv4Addr>,
) -> Option<Ipv4Addr> {
    candidates.iter().copied().max_by_key(|&c| {
        (
            local_ips
                .iter()
                .map(|&l| prefix_bits(c, l))
                .max()
                .unwrap_or(0),
            host_declared == Some(c),
            routed_local.map_or(0, |r| prefix_bits(c, r)),
            std::cmp::Reverse(u32::from(c)),
        )
    })
}

/// [`rank_host_addr`] with this machine's live context: every non-loopback unicast IPv4, plus
/// the source address the OS routes toward the internet. Gathered per call — discovery events
/// are rare, and interfaces change (VPN up/down) between them.
pub fn pick_host_addr(
    candidates: &[Ipv4Addr],
    host_declared: Option<Ipv4Addr>,
) -> Option<Ipv4Addr> {
    rank_host_addr(
        candidates,
        host_declared,
        &local_ipv4s(),
        routed_local_ipv4(),
    )
}

fn local_ipv4s() -> Vec<Ipv4Addr> {
    if_addrs::get_if_addrs()
        .map(|ifs| {
            ifs.into_iter()
                .filter_map(|i| match i.ip() {
                    IpAddr::V4(v) if !v.is_loopback() => Some(v),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Same trick as the host's `primary_local_ip`: a UDP `connect()` performs the route lookup
/// without sending a packet, and `local_addr` is the source address the OS chose.
fn routed_local_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v) if !v.is_loopback() => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::rank_host_addr;
    use std::net::Ipv4Addr;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    // The 2026-08-28 field case: host advertises from its LAN address, the OS responder adds
    // the ZeroTier address over the overlay's multicast, and both machines are on both
    // networks. The LAN address must win — with or without the host's TXT declaration.
    #[test]
    fn shared_lan_beats_shared_overlay() {
        let candidates = [ip("192.168.196.206"), ip("192.168.1.170")];
        let locals = [ip("192.168.1.150"), ip("192.168.196.57")];
        for declared in [None, Some(ip("192.168.1.170"))] {
            assert_eq!(
                rank_host_addr(&candidates, declared, &locals, Some(ip("192.168.1.150"))),
                Some(ip("192.168.1.170"))
            );
        }
    }

    // A client that can ONLY reach the host through the overlay (different site): the host's
    // declared LAN address is not on any of our subnets, so it must NOT win — the overlay
    // address is the reachable one.
    #[test]
    fn overlay_only_client_ignores_the_declared_lan_address() {
        let candidates = [ip("192.168.1.170"), ip("192.168.196.206")];
        let locals = [ip("10.1.2.3"), ip("192.168.196.57")];
        assert_eq!(
            rank_host_addr(
                &candidates,
                Some(ip("192.168.1.170")),
                &locals,
                Some(ip("10.1.2.3"))
            ),
            Some(ip("192.168.196.206"))
        );
    }

    // A multi-NIC host (Ethernet + Wi-Fi on the same LAN) ties on every reachability rung;
    // its own declaration settles which of ITS addresses we dial. Without the declaration
    // (older host) the pick is still deterministic.
    #[test]
    fn declared_addr_settles_a_multi_nic_tie() {
        let candidates = [ip("192.168.1.170"), ip("192.168.1.171")];
        let locals = [ip("192.168.1.150")];
        assert_eq!(
            rank_host_addr(
                &candidates,
                Some(ip("192.168.1.171")),
                &locals,
                Some(ip("192.168.1.150"))
            ),
            Some(ip("192.168.1.171"))
        );
        assert_eq!(
            rank_host_addr(&candidates, None, &locals, Some(ip("192.168.1.150"))),
            Some(ip("192.168.1.170")),
            "no declaration: lowest address, never a hash-order roll"
        );
    }

    #[test]
    fn no_context_is_still_deterministic() {
        let candidates = [ip("10.0.0.9"), ip("10.0.0.5")];
        assert_eq!(
            rank_host_addr(&candidates, None, &[], None),
            Some(ip("10.0.0.5"))
        );
        assert_eq!(rank_host_addr(&[], None, &[], None), None);
    }
}
