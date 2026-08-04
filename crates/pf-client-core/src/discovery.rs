//! LAN host discovery: browse the host's mDNS advert (`_punktfunk._udp`, TXT keys
//! `fp`/`pair`/`id` — see the host crate's `discovery.rs`) on a worker thread and stream
//! results to the UI. Removal events are forwarded too, so the hosts page can drop stale
//! cards and flip a saved host's online pip when its advert disappears.

use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    /// Stable row key: the advertised host id, falling back to the mDNS fullname.
    pub key: String,
    /// The mDNS service fullname — what a later `Removed` event identifies the advert by.
    pub fullname: String,
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// Host certificate fingerprint to pin (lowercase hex), empty if not advertised.
    pub fp_hex: String,
    /// Pairing requirement: `"required"` or `"optional"`.
    pub pair: String,
    /// The management API's port (mDNS `mgmt` TXT) — where the game library is served.
    /// `None` when not advertised (older host / standalone `punktfunk1-host`); the
    /// library client then falls back to the well-known default.
    pub mgmt_port: Option<u16>,
    /// Wake-on-LAN MAC(s) from the mDNS `mac` TXT (comma-separated `aa:bb:cc:dd:ee:ff`), which the
    /// hosts page persists onto the matching saved host so it can wake it later. Empty if absent.
    pub mac: Vec<String>,
    /// The host's OS-identity chain from the mDNS `os` TXT (`windows` | `macos` |
    /// `linux[/<family>][/<id>]`), sanitized ([`crate::os::sanitize_os`]) — drives the host
    /// card's OS icon and is persisted like `mac`. Empty if absent (older host).
    pub os: String,
}

impl DiscoveredHost {
    /// The host's advertised stable id (mDNS TXT `id`), or `""` when it doesn't advertise one.
    /// [`DiscoveredHost::key`] falls back to the mDNS fullname in that case, so the two being
    /// equal is exactly the "no id" signal — read it through here rather than re-deriving it.
    pub fn advertised_id(&self) -> &str {
        if self.key == self.fullname {
            ""
        } else {
            &self.key
        }
    }
}

/// One discovery update for the UI's advert map.
pub enum DiscoveryEvent {
    /// A host advert appeared or refreshed (new address, pairing flipped, …).
    Resolved(DiscoveredHost),
    /// The advert went away (host stopped / left the network).
    Removed { fullname: String },
}

/// Browse continuously. The worker exits when the returned receiver is dropped, or when the
/// daemon dies — checked on a tick, so it stops even on a LAN where no advert ever arrives.
pub fn browse() -> async_channel::Receiver<DiscoveryEvent> {
    let (tx, rx) = async_channel::unbounded();
    std::thread::Builder::new()
        .name("punktfunk-mdns".into())
        .spawn(move || {
            let daemon = match ServiceDaemon::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS daemon failed — discovery disabled");
                    return;
                }
            };
            let receiver = match daemon.browse("_punktfunk._udp.local.") {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS browse failed — discovery disabled");
                    return;
                }
            };
            // Polled rather than blocked on: the worker has to notice that its consumer went
            // away even when NOTHING is arriving, which is the normal state of a LAN with no
            // hosts on it. A plain `recv()` parks forever there, and the ignored-event arm below
            // never touches `tx` — so a bounded consumer like `discover_for` would leak this
            // thread and its daemon (another thread, and a socket bound to :5353) on every call.
            loop {
                // Checked at the TOP so it also covers the arms below that `continue` without
                // ever touching `tx` — the ignored event kinds, and an advert with no IPv4
                // address. Those are the paths that would otherwise keep this thread alive with
                // nobody to send to.
                if tx.is_closed() {
                    break;
                }
                let event = match receiver.recv_timeout(Duration::from_millis(250)) {
                    Ok(event) => event,
                    Err(_) if receiver.is_disconnected() => break,
                    Err(_) => continue,
                };
                let update = match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let props = info.get_properties();
                        let val = |k: &str| props.get_property_val_str(k).unwrap_or("").to_string();
                        // IPv4 only, on purpose (same policy as the Android client): the core
                        // dials `format!("{host}:{port}").parse::<SocketAddr>()` over IPv4-bound
                        // sockets, so a v6 pick from this unordered address set (the host's OS
                        // responder often answers AAAA for its hostname) would render a host card
                        // that fails on every click. A v6-only advert is dropped — the honest
                        // "not found" — until the stack actually speaks IPv6.
                        let Some(addr) =
                            info.get_addresses_v4().iter().next().map(|a| a.to_string())
                        else {
                            continue;
                        };
                        let id = val("id");
                        DiscoveryEvent::Resolved(DiscoveredHost {
                            key: if id.is_empty() {
                                info.get_fullname().to_string()
                            } else {
                                id
                            },
                            fullname: info.get_fullname().to_string(),
                            name: info
                                .get_fullname()
                                .split('.')
                                .next()
                                .unwrap_or("?")
                                .to_string(),
                            addr,
                            port: info.get_port(),
                            fp_hex: val("fp"),
                            pair: val("pair"),
                            mgmt_port: val("mgmt").parse().ok(),
                            mac: val("mac")
                                .split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect(),
                            os: crate::os::sanitize_os(&val("os")),
                        })
                    }
                    ServiceEvent::ServiceRemoved(_ty, fullname) => {
                        DiscoveryEvent::Removed { fullname }
                    }
                    _ => continue,
                };
                if tx.send_blocking(update).is_err() {
                    break; // UI gone — stop browsing
                }
            }
            let _ = daemon.shutdown();
        })
        .expect("spawn mdns thread");
    rx
}

/// The advert map one browse window folded down to. Kept separate from [`discover_for`] so the
/// fold — which is where dedupe and removal actually live — is testable without a network.
type Adverts = BTreeMap<String, DiscoveredHost>;

/// Apply one event to the map. A refreshed advert WINS over the one already there (it carries
/// the newer address — a host that changed DHCP lease re-announces), and a removal drops
/// whichever entry that mDNS fullname produced, whatever it was keyed under.
fn fold(adverts: &mut Adverts, event: DiscoveryEvent) {
    match event {
        DiscoveryEvent::Resolved(host) => {
            adverts.insert(host.key.clone(), host);
        }
        DiscoveryEvent::Removed { fullname } => {
            adverts.retain(|_, h| h.fullname != fullname);
        }
    }
}

/// Browse for `timeout`, then return what answered — deduped by `key`, address-sorted.
///
/// Blocking; intended for one-shot consumers (the CLI's `discover` verb, a plugin backend that
/// wants one bounded call rather than a stream). The streaming [`browse`] stays the UI's door:
/// a live hosts page wants adverts as they land, not a snapshot taken `timeout` after it opened.
pub fn discover_for(timeout: Duration) -> Vec<DiscoveredHost> {
    let rx = browse();
    let deadline = Instant::now() + timeout;
    let mut adverts = Adverts::new();
    while Instant::now() < deadline {
        while let Ok(event) = rx.try_recv() {
            fold(&mut adverts, event);
        }
        // A short tick rather than a blocking recv with a deadline: `async_channel`'s blocking
        // receive has no timeout, and the whole point of this call is that it is bounded.
        std::thread::sleep(Duration::from_millis(50).min(timeout));
    }
    while let Ok(event) = rx.try_recv() {
        fold(&mut adverts, event);
    }
    // Dropping the receiver is what stops the worker — it polls for that, so this holds even
    // when nothing is advertising. Without it a one-shot consumer would leak a browse per call.
    drop(rx);
    sorted(adverts)
}

/// The map as the list a caller gets: sorted by address, then port. IPv4 is compared
/// NUMERICALLY (a lexical sort puts `.10` before `.9`, which reads as scrambled in a host list).
fn sorted(adverts: Adverts) -> Vec<DiscoveredHost> {
    let mut hosts: Vec<DiscoveredHost> = adverts.into_values().collect();
    hosts.sort_by_key(|h| {
        (
            h.addr.parse::<std::net::Ipv4Addr>().ok().map(u32::from),
            h.addr.clone(),
            h.port,
        )
    });
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(key: &str, fullname: &str, addr: &str) -> DiscoveredHost {
        DiscoveredHost {
            key: key.into(),
            fullname: fullname.into(),
            name: fullname.split('.').next().unwrap_or("?").into(),
            addr: addr.into(),
            port: 9777,
            fp_hex: "aa".into(),
            pair: "required".into(),
            mgmt_port: Some(47990),
            mac: vec![],
            os: String::new(),
        }
    }

    /// Two adverts for the same host collapse to one row, and the LATER one wins — that is how
    /// a host that moved to a new address stops being listed at the stale one.
    #[test]
    fn refreshed_advert_supersedes_the_earlier_one() {
        let mut adverts = Adverts::new();
        fold(
            &mut adverts,
            DiscoveryEvent::Resolved(host("id-1", "desk._punktfunk._udp.local.", "192.168.1.9")),
        );
        fold(
            &mut adverts,
            DiscoveryEvent::Resolved(host("id-1", "desk._punktfunk._udp.local.", "192.168.1.20")),
        );
        let out = sorted(adverts);
        assert_eq!(out.len(), 1, "same key must not render twice");
        assert_eq!(out[0].addr, "192.168.1.20", "the newer address wins");
    }

    /// A host that goes away during the browse window is not in the answer.
    #[test]
    fn removal_drops_the_advert_it_names() {
        let mut adverts = Adverts::new();
        fold(
            &mut adverts,
            DiscoveryEvent::Resolved(host("id-1", "desk._punktfunk._udp.local.", "192.168.1.9")),
        );
        fold(
            &mut adverts,
            DiscoveryEvent::Resolved(host("id-2", "tv._punktfunk._udp.local.", "192.168.1.10")),
        );
        fold(
            &mut adverts,
            DiscoveryEvent::Removed {
                fullname: "desk._punktfunk._udp.local.".into(),
            },
        );
        let out = sorted(adverts);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, "id-2");
    }

    /// A host with no `id` TXT is keyed by its fullname — and must not then report that
    /// fullname as an id, which would send a caller launching against a nonexistent reference.
    #[test]
    fn advertised_id_is_empty_without_the_txt() {
        let named = host("id-1", "desk._punktfunk._udp.local.", "10.0.0.1");
        assert_eq!(named.advertised_id(), "id-1");
        let anonymous = host(
            "desk._punktfunk._udp.local.",
            "desk._punktfunk._udp.local.",
            "10.0.0.1",
        );
        assert_eq!(anonymous.advertised_id(), "");
    }

    /// Addresses sort the way a person reads them, not the way strings compare.
    #[test]
    fn addresses_sort_numerically() {
        let mut adverts = Adverts::new();
        for (i, addr) in ["192.168.1.20", "192.168.1.9", "192.168.1.100"]
            .into_iter()
            .enumerate()
        {
            fold(
                &mut adverts,
                DiscoveryEvent::Resolved(host(&format!("id-{i}"), &format!("h{i}."), addr)),
            );
        }
        let out = sorted(adverts);
        let addrs: Vec<&str> = out.iter().map(|h| h.addr.as_str()).collect();
        assert_eq!(addrs, ["192.168.1.9", "192.168.1.20", "192.168.1.100"]);
    }
}
