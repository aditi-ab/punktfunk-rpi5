//! mDNS browse of `_punktfunk._udp` (TXT `fp`/`pair`/`id`; host crate
//! `discovery.rs`). A worker streams [`DiscoveryEvent`]; it exits when the
//! receiver is dropped, polled so an empty LAN still stops.

use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// DNS-SD type hosts advertise. See host crate `punktfunk_host::discovery`.
const SERVICE_TYPE: &str = "_punktfunk._udp.local.";

#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    /// Advertised host id, or the mDNS fullname when `id` is absent.
    pub key: String,
    /// mDNS service fullname. [`DiscoveryEvent::Removed`] names the advert by this.
    pub fullname: String,
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// Certificate fingerprint to pin (lowercase hex). Empty if not advertised.
    pub fp_hex: String,
    /// `"required"` or `"optional"`.
    pub pair: String,
    /// Management API port from mDNS `mgmt`. `None` if absent; the library
    /// client then uses the well-known default.
    pub mgmt_port: Option<u16>,
    /// Wake-on-LAN MACs from mDNS `mac` (comma-separated `aa:bb:cc:dd:ee:ff`). Empty if absent.
    pub mac: Vec<String>,
    /// OS-identity chain from mDNS `os` (`windows` | `macos` | `linux[/<family>][/<id>]`),
    /// sanitized ([`crate::os::sanitize_os`]). Empty if absent.
    pub os: String,
}

impl DiscoveredHost {
    /// Advertised mDNS TXT `id`, or `""` when absent. [`DiscoveredHost::key`] then
    /// equals `fullname` — that equality is the no-id signal; use this, do not re-derive.
    pub fn advertised_id(&self) -> &str {
        if self.key == self.fullname {
            ""
        } else {
            &self.key
        }
    }
}

pub enum DiscoveryEvent {
    /// Appeared or refreshed (new address, pairing, …).
    Resolved(DiscoveredHost),
    Removed {
        fullname: String,
    },
}

/// Cheap-to-clone flag: force an immediate re-query. A request after the
/// browse ends is never read.
///
/// `mdns-sd` re-queries on a doubling backoff (1s, 2s, 4s … cap 1h), so a
/// long-lived browse is passive. Re-querying resets that clock.
#[derive(Clone, Debug)]
pub struct Rescan(Arc<AtomicBool>);

impl Rescan {
    /// Set the flag; the query follows within a tick. Coalesces: many requests, one query.
    pub fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Continuous browse plus [`Rescan`]. Worker exits when the receiver is
/// dropped or the daemon dies — polled on a tick, so an empty LAN still stops.
pub fn browse() -> (async_channel::Receiver<DiscoveryEvent>, Rescan) {
    let (tx, rx) = async_channel::unbounded();
    let flag = Arc::new(AtomicBool::new(false));
    let requested = flag.clone();
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
            let mut receiver = match daemon.browse(SERVICE_TYPE) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "mDNS browse failed — discovery disabled");
                    return;
                }
            };
            // Poll, do not `recv()`: no adverts is the empty-LAN case and
            // ignored events never touch `tx`. A blocking recv would leak this
            // thread, the daemon, and :5353 on every `discover_for` call.
            loop {
                // Before the `continue` arms that never send (ignored events,
                // no IPv4). Those would otherwise keep the thread with no consumer.
                if tx.is_closed() {
                    break;
                }
                // Same placement: every `continue` below would skip the swap.
                if requested.swap(false, Ordering::Relaxed) {
                    // Re-browse REPLACES the listener: replays the cache, puts a
                    // PTR on the wire now, and resets the `Rescan` backoff.
                    match daemon.browse(SERVICE_TYPE) {
                        Ok(r) => receiver = r,
                        Err(e) => tracing::warn!(error = %e, "mDNS rescan failed"),
                    }
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
                        // IPv4 only: the core dials `{host}:{port}` on IPv4-bound
                        // sockets; a v6 pick from this unordered set fails on click.
                        // Among v4, `pick_host_addr` — `HashSet::iter().next()` is
                        // an arbitrary overlay vs LAN address.
                        let candidates: Vec<std::net::Ipv4Addr> =
                            info.get_addresses_v4().into_iter().collect();
                        let Some(addr) = punktfunk_core::discovery::pick_host_addr(
                            &candidates,
                            val("addr").parse().ok(),
                        )
                        .map(|a| a.to_string()) else {
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
                    break;
                }
            }
            let _ = daemon.shutdown();
        })
        .expect("spawn mdns thread");
    (rx, Rescan(flag))
}

/// Folded advert map. Separate from [`discover_for`] so fold is testable offline.
type Adverts = BTreeMap<String, DiscoveredHost>;

/// Refresh wins (newer address). Removal drops by mDNS fullname, not `key`.
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

/// Blocking one-shot: browse `timeout`, return deduped-by-`key`, address-sorted.
/// Live UIs want [`browse`]; this is for CLI `discover` and plugin backends.
pub fn discover_for(timeout: Duration) -> Vec<DiscoveredHost> {
    let (rx, _rescan) = browse();
    let deadline = Instant::now() + timeout;
    let mut adverts = Adverts::new();
    while Instant::now() < deadline {
        while let Ok(event) = rx.try_recv() {
            fold(&mut adverts, event);
        }
        // Tick, not blocking recv: `async_channel` has no timeout and this call is bounded.
        std::thread::sleep(Duration::from_millis(50).min(timeout));
    }
    while let Ok(event) = rx.try_recv() {
        fold(&mut adverts, event);
    }
    // Dropping `rx` is what stops the worker (it polls `tx.is_closed()`).
    // Without this, a one-shot leaks a browse per call even on an empty LAN.
    drop(rx);
    sorted(adverts)
}

/// Address then port. IPv4 numeric: lexical sort puts `.10` before `.9`.
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

    /// No `id` TXT → keyed by fullname; `advertised_id` must still be `""`,
    /// or a launch would target a nonexistent reference.
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
