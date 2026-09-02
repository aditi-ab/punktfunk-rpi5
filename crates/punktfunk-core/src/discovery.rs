//! Which A record to dial when an mDNS advert resolves to several.
//!
//! The resolved set is a union of answers from every responder on every
//! interface. The host advert registers one address (its routed primary;
//! host crate `discovery.rs`), but the OS mDNS responder also answers
//! `<host>.local.` per interface. Overlay networks add their addresses
//! to the same set.
//!
//! [`rank_host_addr`] is the pure policy; [`pick_host_addr`] applies it
//! with this machine's live context.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// Shared leading bits — the ranking's on-link proxy. No netmasks: a longer
/// prefix with one of our addresses is "more on this segment".
fn prefix_bits(a: Ipv4Addr, b: Ipv4Addr) -> u32 {
    (u32::from(a) ^ u32::from(b)).leading_zeros()
}

/// Address to dial, chosen deterministically. Best score wins:
///
/// 1. longest common prefix with any of this machine's unicast addresses
///    (on-link beats routed; overlay wins only when we have no LAN match);
/// 2. the address the host declared (mDNS TXT `addr`);
/// 3. longest common prefix with our default-route source;
/// 4. numerically lowest — so a re-announce cannot flap the pick.
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

/// [`rank_host_addr`] with live context: non-loopback unicast IPv4 plus the
/// OS default-route source. Gathered per call — interfaces change between
/// discovery events.
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

/// UDP `connect` does a route lookup without sending; `local_addr` is the
/// source the OS chose. Same as the host's `primary_local_ip`.
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

    // Shared LAN + overlay: LAN must win, with or without the host's TXT declaration.
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

    // Overlay-only client: declared LAN is off-link, so it must not beat the overlay address.
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

    // Same-LAN multi-NIC tie: host declaration wins; without it, lowest address.
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
